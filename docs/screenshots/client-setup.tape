# Nexdesk Client Setup (macOS) — full install + setup flow
# Requires: nexdesk server running on the network
# Run: vhs docs/screenshots/client-setup.tape

Output docs/screenshots/client-setup.gif

Set Shell zsh
Set FontSize 16
Set Width 900
Set Height 500
Set Theme "Catppuccin Mocha"
Set Padding 20

# Install
Type "curl -fsSL https://raw.githubusercontent.com/tapthaker/nexdesk/master/install.sh | sh"
Enter
Sleep 8s
Screenshot docs/screenshots/client-00-install.png

# PATH prompt — accept default (Y)
Type "y"
Enter
Sleep 2s

# Setup wizard launches automatically
Sleep 2s

# Step 1: Welcome
Screenshot docs/screenshots/client-01-welcome.png
Sleep 500ms

# Step 2: Role — select Client
Enter
Sleep 1s
Down
Sleep 500ms
Screenshot docs/screenshots/client-02-role.png
Sleep 500ms

# Step 3: Network — wait for peer discovery
Enter
Sleep 5s
Screenshot docs/screenshots/client-03-network.png
Sleep 500ms

# Step 4: Certificates
Enter
Sleep 1s
Screenshot docs/screenshots/client-04-certificates.png
Sleep 500ms

# Step 5: Permissions (macOS Accessibility)
Enter
Sleep 3s
Screenshot docs/screenshots/client-05-permissions.png
Sleep 500ms

# Step 6: Service Installation
Enter
Sleep 2s
Screenshot docs/screenshots/client-06-service.png
Sleep 500ms

# Done
Enter
Sleep 2s
Screenshot docs/screenshots/client-07-done.png
Sleep 500ms

# Exit
Type "q"
Sleep 500ms
