API_SOCKET="/firecracker/tmp/firecracker_test0.socket"
LOGFILE="/firecracker/tmp/firecracker.log"

# Set log file
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"log_path\": \"${LOGFILE}\",
        \"level\": \"Debug\",
        \"show_level\": true,
        \"show_log_origin\": true
    }" \
    "http://localhost/logger"