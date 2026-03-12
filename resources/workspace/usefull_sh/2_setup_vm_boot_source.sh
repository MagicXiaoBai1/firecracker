API_SOCKET="/firecracker/tmp/firecracker_test0.socket"

KERNEL="/firecracker/resources/workspace/guest/vmlinux-6.1.155"
KERNEL_BOOT_ARGS="console=ttyS0 reboot=k panic=1"

ARCH=$(uname -m)

if [ ${ARCH} = "aarch64" ]; then
    KERNEL_BOOT_ARGS="keep_bootcon ${KERNEL_BOOT_ARGS}"
fi

# Set boot source
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"kernel_image_path\": \"${KERNEL}\",
        \"boot_args\": \"${KERNEL_BOOT_ARGS}\"
    }" \
    "http://localhost/boot-source"
