# API requests are handled asynchronously, it is important the microVM has been
# started before we attempt to SSH into it.
sleep 2s

KEY_NAME=./$(ls *.id_rsa | tail -1)

# Setup internet access in the guest
ssh -i $KEY_NAME root@172.16.0.2  "ip route add default via 172.16.0.1 dev eth0"

# Setup DNS resolution in the guest
ssh -i $KEY_NAME root@172.16.0.2  "echo 'nameserver 8.8.8.8' > /etc/resolv.conf"

# SSH into the microVM
ssh -i $KEY_NAME root@172.16.0.2

# Use `root` for both the login and password.
# Run `reboot` to exit.