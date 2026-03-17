# Authentication Checklist

Use this checklist before you connect from your iPad.

## Recommended Order

1. Verify that plain `ssh username@host` works from another machine first.
2. Decide whether you want to use an SSH key, a password, or both.
3. Keep the same username, host, and port in Zed for iPad that work in your terminal.

## Passwords

- Zed for iPad can prompt for your SSH password when the server asks for it.
- You can optionally remember that secret in the system credential store for future connections.
- If authentication suddenly fails, re-check the account password on the remote machine before changing app settings.

## SSH Keys

- If your server expects a private key, add the matching SSH argument in the advanced options field.
- Example:

```bash
-i ~/.ssh/id_ed25519
```

- If you use a non-default port or extra proxy/jump-host flags, add them in the same advanced options area.

## Host Verification

- On first connection, Zed learns the remote host key like a normal SSH client.
- If a known host key changes later, the app will reject the connection until you fix the stale `known_hosts` entry.

## Good Defaults

- Start with the simplest working SSH command.
- Add port forwards only after the base connection is reliable.
- Turn on "remember password" only for machines you trust.
