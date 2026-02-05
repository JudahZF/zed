# Start the Remote Server

The Zed remote server allows your iPad to connect and edit code on your development machine.

## Start the Server

Open a terminal on your remote machine and run:

```bash
zed --remote-server
```

The server will start and display a status message indicating it's ready.

## Keep the Server Running

For the best experience, keep the terminal window open while using Zed for iPad. You can also run the server in the background:

```bash
zed --remote-server &
```

Or use a terminal multiplexer like `tmux` or `screen`:

```bash
tmux new -d -s zed 'zed --remote-server'
```

## Automatic Startup (Optional)

If you want the remote server to start automatically when you log in, you can add it to your shell profile or system startup scripts.

### macOS

Add to `~/.zshrc` or `~/.bash_profile`:

```bash
# Start Zed remote server in background
zed --remote-server &>/dev/null &
```

### Linux (systemd)

Create a user service at `~/.config/systemd/user/zed-remote.service`:

```ini
[Unit]
Description=Zed Remote Server
After=network.target

[Service]
ExecStart=zed --remote-server
Restart=always

[Install]
WantedBy=default.target
```

Then enable it:

```bash
systemctl --user enable zed-remote
systemctl --user start zed-remote
```

## Next Steps

Now you'll need to ensure SSH is set up correctly so your iPad can connect to this machine.
