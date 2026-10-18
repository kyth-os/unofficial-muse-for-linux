//! Confined Linux filesystem capabilities for an explicitly authorized Muse host.
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const LIMIT: usize = 65536;
#[derive(Clone, Debug)]
pub struct GrantInfo {
    pub id: String,
    pub label: String,
    pub root: PathBuf,
}
#[derive(Clone, Debug)]
pub struct Invocation {
    pub id: String,
    pub command: String,
    pub params: Value,
    pub timeout_ms: u64,
}
#[derive(Clone, Debug)]
pub struct ToolReply {
    pub id: String,
    pub ok: bool,
    pub output: Value,
    pub error: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub invocation_id: String,
    pub title: String,
    pub path: String,
    pub content: String,
    pub expires_in_seconds: u64,
}
#[derive(Clone, Debug)]
pub enum Decision {
    Reply(ToolReply),
    Approval(ApprovalRequest),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolError(pub &'static str);
impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ToolError {}
struct Grant {
    info: GrantInfo,
    fd: Arc<File>,
}
struct Pending {
    request: ApprovalRequest,
    root: Arc<File>,
    deadline: Instant,
    epoch: u64,
}
#[derive(Default)]
struct State {
    grants: HashMap<String, Grant>,
    seen: HashSet<String>,
    pending: HashMap<String, Pending>,
    nonce: String,
    epoch: u64,
    next: u64,
}
#[cfg(test)]
type Pause = Arc<(std::sync::Barrier, std::sync::Barrier)>;
#[derive(Clone)]
pub struct ToolBroker {
    state: Arc<Mutex<State>>,
    #[cfg(test)]
    pause: Arc<Mutex<Option<Pause>>>,
}
fn fail(id: &str, code: &str) -> ToolReply {
    ToolReply {
        id: id.into(),
        ok: false,
        output: Value::Null,
        error: Some(code.into()),
    }
}
fn good(id: &str, output: Value) -> ToolReply {
    ToolReply {
        id: id.into(),
        ok: true,
        output,
        error: None,
    }
}
fn cstr(s: &str) -> Result<CString, ToolError> {
    CString::new(s).map_err(|_| ToolError("invalid_path"))
}
fn valid_path(s: &str) -> Result<(), ToolError> {
    if s.is_empty()
        || s.len() > 4096
        || s.split('/').any(|x| x.is_empty() || x == "." || x == "..")
        || s.contains('\0')
    {
        Err(ToolError("invalid_path"))
    } else {
        Ok(())
    }
}
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
fn open_beneath(root: &File, path: &str, directory: bool) -> Result<File, ToolError> {
    valid_path(path)?;
    let path = cstr(path)?;
    let how = OpenHow {
        flags: (libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 }) as u64,
        mode: 0,
        resolve: 0x08 | 0x04 | 0x02,
    };
    // SAFETY: pointers refer to initialized, live buffers; returned fd is uniquely owned.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(ToolError("io_denied"));
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    if !directory
        && !file
            .metadata()
            .map_err(|_| ToolError("io_denied"))?
            .is_file()
    {
        return Err(ToolError("special_file"));
    }
    Ok(file)
}
fn read_text(root: &File, path: &str) -> Result<Value, ToolError> {
    let file = open_beneath(root, path, false)?;
    if file.metadata().map_err(|_| ToolError("io_denied"))?.len() > LIMIT as u64 {
        return Err(ToolError("too_large"));
    }
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError("io_denied"))?;
    if bytes.len() > LIMIT {
        return Err(ToolError("too_large"));
    }
    let content = String::from_utf8(bytes).map_err(|_| ToolError("invalid_utf8"))?;
    Ok(json!({"path":path,"content":content}))
}
impl ToolBroker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                nonce: fresh_nonce(),
                ..State::default()
            })),
            #[cfg(test)]
            pause: Arc::default(),
        }
    }
    pub fn grant(&self, root: &Path) -> Result<GrantInfo, ToolError> {
        if root.as_os_str().is_empty() {
            return Err(ToolError("invalid_path"));
        }
        let epoch = self.state.lock().unwrap().epoch;
        // Open the supplied root without following any component's symlink.
        let base = File::open(if root.is_absolute() { "/" } else { "." })
            .map_err(|_| ToolError("io_denied"))?;
        let relative = if root.is_absolute() {
            root.strip_prefix("/")
                .map_err(|_| ToolError("invalid_path"))?
        } else {
            root
        };
        let fd = if relative.as_os_str().is_empty() {
            base
        } else {
            open_beneath(
                &base,
                relative.to_str().ok_or(ToolError("invalid_path"))?,
                true,
            )?
        };
        #[cfg(test)]
        self.test_pause();
        let mut state = self.state.lock().unwrap();
        if state.epoch != epoch {
            return Err(ToolError("revoked"));
        }
        state.next += 1;
        let info = GrantInfo {
            id: format!("grant-{}-{}", state.nonce, state.next),
            label: format!("Folder {}", state.next),
            root: root.to_path_buf(),
        };
        state.grants.insert(
            info.id.clone(),
            Grant {
                info: info.clone(),
                fd: Arc::new(fd),
            },
        );
        Ok(info)
    }
    pub fn grants(&self) -> Vec<GrantInfo> {
        let mut grants: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .grants
            .values()
            .map(|x| x.info.clone())
            .collect();
        grants.sort_by(|a, b| a.id.cmp(&b.id));
        grants
    }
    pub fn manifest(&self) -> Value {
        let grants: Vec<_> = self
            .grants()
            .into_iter()
            .map(|g| json!({"id":g.id,"label":g.label}))
            .collect();
        let grant_ids: Vec<_> = grants.iter().map(|grant| grant["id"].clone()).collect();
        let grant_description = format!(
            "Granted folder ID. Available user-authorized folders (ID and label): {}",
            serde_json::to_string(&grants).expect("grant metadata is serializable")
        );
        let mut commands = serde_json::Map::new();
        for (name,description,fields) in [
            ("files.search","Search filenames in an explicitly granted folder.",vec![("grant_id","Granted folder ID"),("query","Case-sensitive filename substring; max 256 bytes")]),
            ("files.read_text","Read at most 64 KiB of UTF-8 text inside a granted folder.",vec![("grant_id","Granted folder ID"),("path","Strict relative path")]),
            ("files.create_text","Propose a NEW UTF-8 text file. Always requires local approval of exact path and content; never overwrite.",vec![("grant_id","Granted folder ID"),("path","Strict relative new-file path; parent must exist"),("content","Exact UTF-8 text, at most 65536 bytes")]),
        ] {
            let required: serde_json::Map<String, Value> = fields
                .iter()
                .map(|(key, description)| {
                    let parameter = if *key == "grant_id" {
                        json!({"type":"string","description":grant_description,"enum":grant_ids})
                    } else {
                        json!({"type":"string","description":description})
                    };
                    (key.to_string(), parameter)
                })
                .collect();
            let names:Vec<_>=fields.iter().map(|(key,_)|*key).collect();
            commands.insert(name.into(),json!({"description":description,"required":required,"optional":{},"timeout_ms":120000,"parameters":{"type":"object","properties":required,"required":names,"additionalProperties":false}}));
        }
        json!({"commands_v2":commands,"grants":grants})
    }
    pub fn invoke(&self, invocation: Invocation) -> Decision {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(invocation.timeout_ms.min(120_000));
        let (root, epoch) = {
            let mut state = self.state.lock().unwrap();
            if invocation.id.is_empty() || invocation.id.len() > 256 {
                return Decision::Reply(fail(&invocation.id, "invalid_id"));
            }
            if state.seen.contains(&invocation.id) {
                return Decision::Reply(fail(&invocation.id, "duplicate_id"));
            }
            // Never evict accepted IDs: after capacity, reject all new IDs until broker replacement.
            if state.seen.len() >= 4096 {
                return Decision::Reply(fail(&invocation.id, "capacity"));
            }
            state.seen.insert(invocation.id.clone());
            let result = (|| {
                let keys: &[&str] = match invocation.command.as_str() {
                    "files.read_text" => &["grant_id", "path"],
                    "files.search" => &["grant_id", "query"],
                    "files.create_text" => &["grant_id", "path", "content"],
                    _ => return Err(ToolError("unknown_command")),
                };
                let params = invocation
                    .params
                    .as_object()
                    .ok_or(ToolError("invalid_params"))?;
                if params.len() != keys.len()
                    || keys
                        .iter()
                        .any(|key| params.get(*key).and_then(Value::as_str).is_none())
                {
                    return Err(ToolError("invalid_params"));
                }
                if Instant::now() >= deadline {
                    return Err(ToolError("expired"));
                }
                let grant = state
                    .grants
                    .get(params["grant_id"].as_str().unwrap())
                    .ok_or(ToolError("not_granted"))?;
                if invocation.command == "files.search" {
                    let query = params["query"].as_str().unwrap();
                    if query.len() > 256 || query.contains(['/', '\0']) {
                        return Err(ToolError("invalid_params"));
                    }
                } else {
                    valid_path(params["path"].as_str().unwrap())?;
                }
                Ok((grant.fd.clone(), state.epoch))
            })();
            match result {
                Ok(v) => v,
                Err(e) => return Decision::Reply(fail(&invocation.id, e.0)),
            }
        };
        if invocation.command == "files.create_text" {
            let path = invocation.params["path"].as_str().unwrap();
            let content = invocation.params["content"].as_str().unwrap();
            if content.len() > LIMIT {
                return Decision::Reply(fail(&invocation.id, "too_large"));
            }
            let approval_id = format!("approval-{}", fresh_nonce());
            let mut state = self.state.lock().unwrap();
            if state.epoch != epoch {
                return Decision::Reply(fail(&invocation.id, "revoked"));
            }
            if Instant::now() >= deadline {
                return Decision::Reply(fail(&invocation.id, "expired"));
            }
            if state.pending.len() >= 128 {
                return Decision::Reply(fail(&invocation.id, "capacity"));
            }
            let request = ApprovalRequest {
                approval_id,
                invocation_id: invocation.id.clone(),
                title: "Create a new text file".into(),
                path: path.into(),
                content: content.into(),
                expires_in_seconds: deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .div_ceil(1000) as u64,
            };
            state.pending.insert(
                request.approval_id.clone(),
                Pending {
                    request: request.clone(),
                    root,
                    deadline,
                    epoch,
                },
            );
            return Decision::Approval(request);
        }
        let result = if invocation.command == "files.search" {
            search(
                &root,
                invocation.params["query"].as_str().unwrap(),
                deadline,
            )
        } else {
            read_text(&root, invocation.params["path"].as_str().unwrap())
        };
        #[cfg(test)]
        self.test_pause();
        let state = self.state.lock().unwrap();
        Decision::Reply(if state.epoch != epoch {
            fail(&invocation.id, "revoked")
        } else if Instant::now() >= deadline {
            fail(&invocation.id, "expired")
        } else {
            match result {
                Ok(v) => good(&invocation.id, v),
                Err(e) => fail(&invocation.id, e.0),
            }
        })
    }
    pub fn deny(&self, approval_id: &str) -> ToolReply {
        match self.state.lock().unwrap().pending.remove(approval_id) {
            Some(p) => fail(&p.request.invocation_id, "denied"),
            None => fail("", "stale_approval"),
        }
    }
    pub fn revoke_all(&self) -> Vec<ToolReply> {
        let mut state = self.state.lock().unwrap();
        state.epoch += 1;
        state.grants.clear();
        let mut replies: Vec<_> = state
            .pending
            .drain()
            .map(|(_, p)| fail(&p.request.invocation_id, "revoked"))
            .collect();
        replies.sort_by(|a, b| a.id.cmp(&b.id));
        replies
    }
    pub fn expire(&self) -> Vec<ToolReply> {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let ids: Vec<_> = state
            .pending
            .iter()
            .filter(|(_, p)| now >= p.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        let mut replies: Vec<_> = ids
            .into_iter()
            .map(|id| {
                fail(
                    &state.pending.remove(&id).unwrap().request.invocation_id,
                    "expired",
                )
            })
            .collect();
        replies.sort_by(|a, b| a.id.cmp(&b.id));
        replies
    }
    pub fn approve(&self, approval_id: &str) -> ToolReply {
        let pending = match self.state.lock().unwrap().pending.remove(approval_id) {
            Some(p) => p,
            None => return fail("", "stale_approval"),
        };
        let id = &pending.request.invocation_id;
        if Instant::now() >= pending.deadline {
            return fail(id, "expired");
        }
        // Stage outside the state lock; no visible name, no overwrite and no cleanup debris.
        let staged = stage(
            &pending.root,
            &pending.request.path,
            &pending.request.content,
        );
        #[cfg(test)]
        self.test_pause();
        let state = self.state.lock().unwrap();
        if state.epoch != pending.epoch {
            return fail(id, "revoked");
        }
        if Instant::now() >= pending.deadline {
            return fail(id, "expired");
        }
        let (parent, file, name) = match staged {
            Ok(v) => v,
            Err(e) => return fail(id, e.0),
        };
        let empty = c"";
        // SAFETY: fds and C strings remain live; an anonymous verified inode is linked exactly once.
        let rc = unsafe {
            libc::linkat(
                file.as_raw_fd(),
                empty.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        };
        if rc != 0 {
            return fail(id, "create_denied");
        }
        good(
            id,
            json!({"path":pending.request.path,"bytes":pending.request.content.len()}),
        )
    }
}

fn stage(root: &File, path: &str, content: &str) -> Result<(File, File, CString), ToolError> {
    valid_path(path)?;
    let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = if parent_path.is_empty() {
        root.try_clone().map_err(|_| ToolError("io_denied"))?
    } else {
        open_beneath(root, parent_path, true)?
    };
    let dot = c".";
    // SAFETY: directory fd and static C string are valid; O_TMPFILE leaves no named debris.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            dot.as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(ToolError("create_denied"));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(content.as_bytes())
        .map_err(|_| ToolError("create_denied"))?;
    file.sync_all().map_err(|_| ToolError("create_denied"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ToolError("create_denied"))?;
    let mut actual = Vec::new();
    (&mut file)
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut actual)
        .map_err(|_| ToolError("create_denied"))?;
    if actual != content.as_bytes() {
        return Err(ToolError("verify_failed"));
    }
    Ok((parent, file, cstr(name)?))
}
struct Dir(*mut libc::DIR);
impl Drop for Dir {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}
fn search(root: &File, query: &str, deadline: Instant) -> Result<Value, ToolError> {
    let mut stack = vec![(String::new(), 0usize)];
    let mut paths = Vec::new();
    let mut scanned = 0usize;
    while let Some((relative, depth)) = stack.pop() {
        let directory = if relative.is_empty() {
            // Open a fresh description: dup() would share readdir offsets across calls.
            let how = OpenHow {
                flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY) as u64,
                mode: 0,
                resolve: 0x08 | 0x04 | 0x02,
            };
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    root.as_raw_fd(),
                    c".".as_ptr(),
                    &how,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            if fd < 0 {
                return Err(ToolError("io_denied"));
            }
            unsafe { File::from_raw_fd(fd as i32) }
        } else {
            open_beneath(root, &relative, true)?
        };
        use std::os::fd::IntoRawFd;
        let fd = directory.into_raw_fd();
        let raw = unsafe { libc::fdopendir(fd) };
        if raw.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(ToolError("io_denied"));
        }
        let dir = Dir(raw);
        loop {
            if Instant::now() >= deadline {
                return Err(ToolError("expired"));
            }
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(dir.0) };
            if entry.is_null() {
                if unsafe { *libc::__errno_location() } != 0 {
                    return Err(ToolError("io_denied"));
                }
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            if scanned == 10_000 {
                return Ok(json!({"paths":paths,"scanned":scanned,"truncated":true}));
            }
            scanned += 1;
            let name_str = name.to_str().map_err(|_| ToolError("invalid_utf8"))?;
            let path = if relative.is_empty() {
                name_str.into()
            } else {
                format!("{relative}/{name_str}")
            };
            valid_path(&path)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    fd,
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(ToolError("io_denied"));
            }
            let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
            match kind {
                libc::S_IFREG => {
                    if name_str.contains(query) {
                        paths.push(path);
                        if paths.len() == 100 {
                            return Ok(json!({"paths":paths,"scanned":scanned,"truncated":true}));
                        }
                    }
                }
                libc::S_IFDIR => {
                    if depth >= 64 {
                        return Err(ToolError("depth_limit"));
                    }
                    stack.push((path, depth + 1));
                }
                _ => return Err(ToolError("special_file")),
            }
        }
    }
    paths.sort();
    Ok(json!({"paths":paths,"scanned":scanned,"truncated":false}))
}
impl Default for ToolBroker {
    fn default() -> Self {
        Self::new()
    }
}
fn fresh_nonce() -> String {
    let mut bytes = [0u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: remaining output buffer is writable. Entropy failure never falls back to guessable IDs.
        let result = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        assert!(result > 0, "entropy_unavailable");
        filled += result as usize;
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[cfg(test)]
impl ToolBroker {
    fn test_pause(&self) {
        let pause = self.pause.lock().unwrap().take();
        if let Some(pause) = pause {
            pause.0.wait();
            pause.1.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn granted_read_is_confined() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello"), "hello").unwrap();
        let broker = ToolBroker::new();
        let grant = broker.grant(dir.path()).unwrap();
        let Decision::Reply(reply) = broker.invoke(Invocation {
            id: "read".into(),
            command: "files.read_text".into(),
            params: json!({"grant_id":grant.id,"path":"hello"}),
            timeout_ms: 1000,
        }) else {
            panic!("read must not prompt")
        };
        assert!(reply.ok);
        assert_eq!(reply.output["content"], "hello");
    }

    fn pause(b: &ToolBroker) -> Arc<(std::sync::Barrier, std::sync::Barrier)> {
        let hook = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        *b.pause.lock().unwrap() = Some(hook.clone());
        hook
    }
    #[test]
    fn revocation_discards_in_flight_read_results() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("file"), "secret").unwrap();
        let b = ToolBroker::new();
        let g = b.grant(d.path()).unwrap();
        let hook = pause(&b);
        let worker = b.clone();
        let task = std::thread::spawn(move || {
            worker.invoke(Invocation {
                id: "read".into(),
                command: "files.read_text".into(),
                params: json!({"grant_id":g.id,"path":"file"}),
                timeout_ms: 10000,
            })
        });
        hook.0.wait();
        b.revoke_all();
        hook.1.wait();
        let Decision::Reply(reply) = task.join().unwrap() else {
            panic!()
        };
        assert_eq!(reply.error.as_deref(), Some("revoked"));
        assert!(reply.output.is_null());
    }
    #[test]
    fn revocation_discards_in_flight_grant() {
        let d = tempfile::tempdir().unwrap();
        let b = ToolBroker::new();
        let hook = pause(&b);
        let worker = b.clone();
        let path = d.path().to_path_buf();
        let task = std::thread::spawn(move || worker.grant(&path));
        hook.0.wait();
        b.revoke_all();
        hook.1.wait();
        assert!(task.join().unwrap().is_err());
        assert!(b.grants().is_empty());
    }
    #[test]
    fn revocation_prevents_staged_create_commit_and_cleans_inode() {
        let d = tempfile::tempdir().unwrap();
        let b = ToolBroker::new();
        let g = b.grant(d.path()).unwrap();
        let Decision::Approval(p) = b.invoke(Invocation {
            id: "create".into(),
            command: "files.create_text".into(),
            params: json!({"grant_id":g.id,"path":"new","content":"secret"}),
            timeout_ms: 10000,
        }) else {
            panic!()
        };
        let hook = pause(&b);
        let worker = b.clone();
        let task = std::thread::spawn(move || worker.approve(&p.approval_id));
        hook.0.wait();
        b.revoke_all();
        hook.1.wait();
        let reply = task.join().unwrap();
        assert_eq!(reply.error.as_deref(), Some("revoked"));
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }
}
