#!/usr/bin/env bash
# Install the external community adapter; never handles authentication.
set -euo pipefail
exec python3 -I - "$@" <<'PY'
import argparse
import json
import os
from pathlib import Path
import pwd
import shutil
import stat
import subprocess
import sys

SOURCE_URL = "https://github.com/kleprevost/muse-mcp.git"
SOURCE_COMMIT = "965cc2f197543a8762d3d7067038da6de962730a"


def fail(message):
    raise RuntimeError(message)


def create_destination(destination, home):
    """Reject symlinks/unsafe ancestors; atomically refuse any existing target."""
    if not destination.is_absolute() or ".." in destination.parts:
        fail("--prefix must be an absolute path without '..'")
    if any(character in str(destination) for character in "\r\n\0"):
        fail("--prefix contains an unsupported control character")
    if destination == home or not destination.is_relative_to(home):
        fail("--prefix must be a new directory beneath your account's home directory")
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    current = Path("/")
    try:
        for part in destination.parent.parts[1:]:
            current /= part
            inside_home = current == home or current.is_relative_to(home)
            try:
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            except FileNotFoundError:
                if not inside_home:
                    fail("an ancestor outside your home directory does not exist")
                os.mkdir(part, 0o700, dir_fd=fd)
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            info = os.fstat(child)
            allowed_owners = {os.getuid()} if inside_home else {0, os.getuid()}
            if info.st_uid not in allowed_owners or stat.S_IMODE(info.st_mode) & 0o022:
                os.close(child)
                fail("install ancestors must have trusted ownership and not be group/other-writable")
            os.close(fd)
            fd = child
        # mkdir rather than exist_ok: do not reuse, overwrite, or remove any target.
        os.mkdir(destination.name, 0o700, dir_fd=fd)
    finally:
        os.close(fd)


def main():
    home = Path(pwd.getpwuid(os.getuid()).pw_dir)
    data_home = Path(os.environ.get("XDG_DATA_HOME", str(home / ".local/share")))
    parser = argparse.ArgumentParser(
        description="Install a pinned external muse-mcp source checkout and private venv. "
                    "Does not sign in, access cookies, or modify existing installations."
    )
    parser.add_argument("--prefix", type=Path,
                        default=data_home / "unofficial-muse-for-linux/account-bridge",
                        help="new absolute installation directory beneath your home")
    parser.add_argument("--python", default=sys.executable,
                        help="Python 3.10+ interpreter used to create the venv")
    args = parser.parse_args()
    if os.getuid() == 0 or os.geteuid() == 0 or os.getuid() != os.geteuid():
        fail("run this installer as your regular user, not root or sudo")
    if sys.version_info < (3, 10):
        fail("the installer requires Python 3.10 or newer")
    git = shutil.which("git")
    python = shutil.which(args.python)
    if git is None or python is None:
        fail("git and the selected Python interpreter must be installed")
    git, python = os.path.abspath(git), os.path.abspath(python)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("GIT_") and key not in {"PYTHONHOME", "PYTHONPATH"}}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
               GIT_TERMINAL_PROMPT="0", PYTHONNOUSERSITE="1")
    subprocess.run([python, "-I", "-c",
                    "import sys; assert sys.version_info >= (3, 10), 'Python 3.10+ required'; import venv"],
                   check=True, env=env)
    destination = args.prefix
    create_destination(destination, home)
    source = destination / "source"
    venv = destination / ".venv"

    def run(command, **kwargs):
        return subprocess.run(command, cwd=destination, env=env, check=True, **kwargs)

    # No upstream installer, repository hooks, sudo, or piped shell is executed.
    run([git, "init", "--quiet", "--template=", str(source)])
    git_command = [git, "-c", "core.hooksPath=/dev/null", "-c", "protocol.file.allow=never",
                   "-C", str(source)]
    run(git_command + ["remote", "add", "origin", SOURCE_URL])
    run(git_command + ["fetch", "--quiet", "--no-tags", "--depth=1", "origin", SOURCE_COMMIT])
    run(git_command + ["checkout", "--quiet", "--detach", "FETCH_HEAD"])
    actual = run(git_command + ["rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    if actual != SOURCE_COMMIT:
        fail("source commit did not match the reviewed pin")
    run([python, "-I", "-m", "venv", str(venv)])
    interpreter = venv / "bin/python"
    run([str(interpreter), "-I", "-m", "pip", "--isolated", "--disable-pip-version-check",
         "install", "--requirement", str(source / "requirements.txt")])
    site = Path(run([str(interpreter), "-I", "-c",
                     "import sysconfig; print(sysconfig.get_path('purelib'))"],
                    capture_output=True, text=True).stdout.strip())
    if not site.is_relative_to(venv):
        fail("venv reported a site-packages directory outside the installation")
    # Keep muse.py beside its descriptors/routes. A wheel alone can misplace data.
    # This .pth contains only a path, never executable import statements.
    with (site / "unofficial_muse_account_source.pth").open("x", encoding="utf-8") as output:
        output.write(str(source) + "\n")
    run([str(interpreter), "-I", "-c",
         "import pathlib, sys; import muse; from muse_mcp.client import MuseClient; "
         "from curl_cffi.requests import WebSocket; "
         "assert pathlib.Path(muse.__file__).parent == pathlib.Path(sys.argv[1]); "
         "assert muse.rq is not None; assert muse.ROUTES; "
         "print('Offline import check passed (source, descriptors, routes, transport).')",
         str(source)])
    frozen = run([str(interpreter), "-I", "-m", "pip", "--isolated", "freeze", "--all"],
                 capture_output=True, text=True).stdout
    (destination / "installed-dependencies.txt").write_text(frozen, encoding="utf-8")
    (destination / "installation.json").write_text(json.dumps({
        "source": SOURCE_URL, "commit": SOURCE_COMMIT, "python": str(interpreter),
        "source_dir": str(source), "authenticated": False,
    }, indent=2) + "\n", encoding="utf-8")
    print(f"Installed external community source: {SOURCE_COMMIT}")
    print(f"Select this Python interpreter in the app: {interpreter}")
    print("No authentication was performed or read. Dependencies remain outside the AppImage.")


if __name__ == "__main__":
    os.umask(0o077)
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        # No auth or credential input is accepted anywhere in this installer.
        print(f"Account bridge setup failed: {error}", file=sys.stderr)
        print("Any partial new installation is retained for inspection; existing paths are never removed.",
              file=sys.stderr)
        sys.exit(1)
PY
