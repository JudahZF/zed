# Connect from Your iPad

You're ready to connect Zed for iPad to your development machine!

## Connection Details

On the connection screen, enter:

- **Hostname**: Your machine's IP address or hostname
  - Example: `192.168.1.100` or `your-mac.local`
  
- **Username**: Your user account on the remote machine
  - Example: `john` or `developer`
  
- **Port**: Usually `22` (the default SSH port)

## Connecting

1. Tap **Connect**
2. Enter your password when prompted
3. Wait for the connection to establish

Once connected, you'll see your remote filesystem and can start editing code!

## Troubleshooting

### Connection Refused

- Make sure SSH is enabled on your remote machine
- Check that the Zed remote server is running (`zed --remote-server`)
- Verify the hostname/IP address is correct
- Ensure your iPad is on the same network (or has internet access to your remote machine)

### Authentication Failed

- Double-check your username and password
- Ensure your user account has SSH access
- Check if your account is locked or disabled

### Timeout

- Verify network connectivity between your iPad and remote machine
- Try pinging the remote machine from another device
- Check if a firewall is blocking port 22

### Server Not Found

- The Zed remote server might not be running
- Start it with `zed --remote-server` on your development machine

## Tips for Best Experience

- Use a stable WiFi connection
- Keep your iPad charged or plugged in during long coding sessions
- Brief network interruptions may allow automatic reconnection (depends on server state and interruption duration)
- Session state (open files, cursor positions) may be preserved when reconnecting to the same server session

> **Note**: Reconnection and session persistence features depend on the remote server maintaining its state. If the server restarts or the session times out, you may need to reconnect manually and reopen your files.

## Getting Help

If you continue to have issues:

- Check the [Zed documentation](https://zed.dev/docs)
- Visit the [Zed community forums](https://github.com/zed-industries/zed/discussions)
- Report bugs at [github.com/zed-industries/zed](https://github.com/zed-industries/zed/issues)

Happy coding!
