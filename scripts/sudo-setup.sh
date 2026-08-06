#!/usr/bin/env bash
# sudo-setup.sh — configure passwordless sudo for current user

echo "Setting up passwordless sudo for user: $(whoami)"
echo "$(whoami) ALL=(ALL:ALL) NOPASSWD: ALL" | sudo tee /etc/sudoers.d/dont-prompt-user > /dev/null
sudo chmod 0440 /etc/sudoers.d/dont-prompt-user
echo "Done!"
