# SSH Configuration

Zed for iPad connects to your development machine via SSH. This page helps you ensure SSH is properly configured.

## Check SSH Server

### macOS

1. Open **System Settings** > **General** > **Sharing**
2. Enable **Remote Login**
3. Note the hostname shown (e.g., `your-mac.local`)

You can also enable SSH from the terminal:

```bash
sudo systemsetup -setremotelogin on
```

### Linux

Most Linux distributions have SSH server installed. To ensure it's running:

```bash
# Debian/Ubuntu
sudo apt install openssh-server
sudo systemctl enable ssh
sudo systemctl start ssh

# Fedora/RHEL
sudo dnf install openssh-server
sudo systemctl enable sshd
sudo systemctl start sshd
```

## Find Your IP Address

You'll need to know your machine's IP address to connect from your iPad.

### On the Same Network

If your iPad and computer are on the same WiFi network:

```bash
# macOS
ipconfig getifaddr en0

# Linux
hostname -I | awk '{print $1}'
```

You can also use your machine's hostname (e.g., `your-computer.local` on macOS).

### Remote Access

If connecting from a different network, you'll need to:

1. Set up port forwarding on your router (port 22 for SSH)
2. Use your public IP address (find it at [whatismyip.com](https://whatismyip.com))
3. Or use a VPN to access your home network

## Test SSH Connection

From another device on your network, test the connection:

```bash
ssh username@your-ip-address
```

If this works, you're ready to connect from your iPad!

## Security Notes

- Use strong passwords or SSH keys
- Consider changing the default SSH port (22) for better security
- Use a firewall to restrict access to trusted IP addresses

## Next Steps

You're almost ready! Head to the final step to connect from your iPad.
