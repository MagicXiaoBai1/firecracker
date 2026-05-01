# API requests are handled asynchronously, it is important the microVM has been
# started before we attempt to SSH into it.
# sleep 2s

KEY_NAME=./$(ls *.id_rsa | tail -1)

# Setup internet access in the guest
ssh -i $KEY_NAME root@172.16.0.2  "ip route add default via 172.16.0.1 dev eth0"

# Setup DNS resolution in the guest
ssh -i $KEY_NAME root@172.16.0.2  "echo 'nameserver 8.8.8.8' > /etc/resolv.conf"

# SSH into the microVM
ssh -i $KEY_NAME root@172.16.0.2

# Use `root` for both the login and password.
# Run `reboot` to exit.

# openEuler12#$
openEuler12#$
abc123!
qemu-system-aarch64 \
  -m 8G \
  -smp 4 \
  -cpu cortex-a72 \
  -machine virt \
  -nographic \
  -cdrom "$ISO" \
  -boot d \
  -rtc base=localtime

  qemu-system-aar

qemu-system-aarch64 \
  -m 8192 -cpu cortex-a76 \
  -smp 8,sockets=4,cores=2 \
  -nographic \
  -device virtio-gpu-pci \
  -device nec-usb-xhci -device usb-mouse -device usb-kbd  \
  -M virt -bios /usr/share/edk2/aarch64/QEMU_EFI.fd \
  -drive if=none,file=/home/yunfei/qemu_workspace/ubuntu.img,id=hd0 \
  -device virtio-blk-device,drive=hd0 -drive if=none,file=/home/yunfei/qemu_workspace/ubuntu-25.10-desktop-arm64.iso,id=cdrom,media=cdrom \
  -device virtio-scsi-device -device scsi-cd,drive=cdrom


qemu-system-aarch64 \
  -m 8192 -cpu cortex-a76 \
  -smp 8,sockets=4,cores=2 \
  -serial mon:stdio \
  -monitor unix:/tmp/qemu-monitor.sock,server,nowait \
  -device virtio-gpu-pci \
  -device nec-usb-xhci -device usb-mouse -device usb-kbd  \
  -M virt -bios /usr/share/edk2/aarch64/QEMU_EFI.fd \
  -drive if=none,file=/home/yunfei/qemu_workspace/ubuntu.img,id=hd0 \
  -device virtio-blk-device,drive=hd0 -drive if=none,file=/home/yunfei/qemu_workspace/ubuntu-25.10-desktop-arm64.iso,id=cdrom,media=cdrom \
  -device virtio-scsi-device -device scsi-cd,drive=cdrom

# 不好
  -device VGA 
  -vga virtio 
  -display none \


screendump /tmp/shot.ppm

virt-install --name=ubuntu_h --vcpus=8 --ram=8192 --disk path=/home/yunfei/qemu_workspace/ubuntu.img,format=qcow2,size=50,bus=virtio --machine virt --cdrom /home/yunfei/qemu_workspace/ubuntu-25.10-desktop-arm64.iso --network bridge=virbr0,model=virtio --force --graphic vnc,listen=0.0.0.0,port=5906 --input type=tablet,bus=usb --input type=keyboard,bus=virtio

xvfb-run -s "-screen 0 1920x1080x24 -ac +extension MIT-SHM" \
x11perf -scroll10 -scroll100 -scroll500 \
        -copywinwin10 -copywinwin100 -copywinwin500 \
        -move -resize -map -unmap \
        -time $TEST_TIME -repeat $REPEAT_COUNT
xvfb-run -s "-screen 0 1920x1080x24 -ac" \
> x11perf -scroll100 -copywinwin100 -move -resize -time 3