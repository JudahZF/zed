# Remote-Only Limits

Zed for iPad currently focuses on remote development over SSH.

## What That Means

- Your source code, tasks, terminals, and language tooling run on the remote machine.
- The iPad app is the interface for that remote workspace.
- The app is best when the remote machine is already configured for development.

## Not In Scope Yet

- Local file editing through the iPad document picker
- Full iPhone layouts
- Desktop-complete debugger and collaboration workflows

## Remote Server Downloads

- The app can download and cache the remote server binary locally when needed.
- First connection can take longer while the server binary is fetched, uploaded, or extracted.
- Later connections should reuse cached artifacts when versions still match.

## Troubleshooting Mindset

- Connection errors usually fall into one of three buckets:
  - SSH/authentication
  - Host verification
  - Remote workspace open failures

- Fix the SSH path first, then confirm the remote path, then add extras like port forwards or AI workflows.
