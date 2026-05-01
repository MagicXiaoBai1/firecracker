API_SOCKET="/home/yunfei/firecracker/resources/workspace/tmp/firecracker_test0.socket"

WWROOTFS="/home/yunfei/cloud-hypervisor/workspace/openeuler-rootfs.ext4"
KERNEL="/home/yunfei/kernel/vmlinux.bin"




# KERNEL_BOOT_ARGS="console=ttyS0 reboot=k panic=1"
KERNEL_BOOT_ARGS="console=ttyS0"

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

curl --unix-socket "${API_SOCKET}" -i  \
  -X PUT 'http://localhost/machine-config' \
  -H 'Accept: application/json'            \
  -H 'Content-Type: application/json'      \
  -d '{
           "vcpu_count": 8,
           "mem_size_mib": 8192
  }'



# Set rootfs
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"drive_id\": \"rootfs\",
        \"path_on_host\": \"${ROOTFS}\",
        \"is_root_device\": true,
        \"is_read_only\": false
    }" \
    "http://localhost/drives/rootfs"
