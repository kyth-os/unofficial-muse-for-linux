'use strict';
let nextId = 1;
const $ = (id) => document.getElementById(id);

function showError(detail) {
  $('err').textContent = detail || '';
}

function renderList(tabs) {
  const table = $('tabs');
  const body = table.querySelector('tbody');
  body.textContent = '';
  for (const t of tabs) {
    const tr = document.createElement('tr');
    // textContent only: tab titles/URLs are never interpreted as HTML.
    for (const key of ['id', 'title', 'url']) {
      const td = document.createElement('td');
      td.textContent = String(t[key]);
      tr.appendChild(td);
    }
    body.appendChild(tr);
  }
  table.hidden = false;
}

function callHost(op, params, onResult) {
  showError('');
  chrome.runtime.sendMessage({ id: nextId++, op, params }, (reply) => {
    if (!reply || !reply.ok) {
      showError(reply && reply.error ? reply.error.detail : 'no reply');
      return;
    }
    onResult(reply.result);
  });
}

$('list').addEventListener('click', () => {
  callHost('tabs.list', {}, (result) => renderList(result.tabs));
});
$('open').addEventListener('click', () => {
  const url = $('openUrl').value;
  callHost('tabs.open', { url }, (result) => renderList([result.tab]));
});
$('close').addEventListener('click', () => {
  const raw = $('closeId').value.trim();
  if (raw === '') {
    showError('tab id is missing');
    return;
  }
  const id = Number(raw);
  if (!Number.isInteger(id) || id < 0) {
    showError('tab id must be an unsigned integer');
    return;
  }
  callHost('tabs.close', { id }, () => $('list').click());
});
