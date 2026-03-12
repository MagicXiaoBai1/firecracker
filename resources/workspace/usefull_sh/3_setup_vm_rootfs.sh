API_SOCKET="/firecracker/tmp/firecracker_test0.socket"

ROOTFS="/firecracker/resources/workspace/guest/ubuntu-24.04.ext4"
# Set rootfs
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"drive_id\": \"rootfs\",
        \"path_on_host\": \"${ROOTFS}\",
        \"is_root_device\": true,
        \"is_read_only\": false
    }" \
    "http://localhost/drives/rootfs"
