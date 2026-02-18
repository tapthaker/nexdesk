# Nexdesk Server Setup (Linux) — full install + setup flow
# Run: vhs docs/screenshots/server-setup.tape

Output docs/screenshots/server-setup.gif

Set Shell zsh
Set FontSize 16
Set Width 900
Set Height 500
Set Theme "Catppuccin Mocha"
Set Padding 20

# Install
Type "curl -fsSL https://raw.githubusercontent.com/tapthaker/nexdesk/main/install.sh | sh"
Enter
Sleep 8s
Screenshot docs/screenshots/server-00-install.png

# PATH prompt — accept default (Y)
Type "y"
Enter
Sleep 2s

# Setup wizard launches automatically
Sleep 2s

# Step 1: Welcome
Screenshot docs/screenshots/server-01-welcome.png
Sleep 500ms

# Step 2: Role (Server is default)
Enter
Sleep 1s
Screenshot docs/screenshots/server-02-role.png
Sleep 500ms

# Step 3: Screen Arrangement (default: right)
Enter
Sleep 1s
Screenshot docs/screenshots/server-03-screens.png
Sleep 500ms

# Step 4: Certificates
Enter
Sleep 1s
Screenshot docs/screenshots/server-04-certificates.png
Sleep 500ms

# Step 5: Service Installation
Enter
Sleep 2s
Screenshot docs/screenshots/server-05-service.png
Sleep 500ms

# Done
Enter
Sleep 2s
Screenshot docs/screenshots/server-06-done.png
Sleep 500ms

# Exit
Type "q"
Sleep 500ms
