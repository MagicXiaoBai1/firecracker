echo "root: start 0" 
sh 0_setup_iptables_for_vm.sh 
echo "root: start 0" 
sh 1_setup_vm_log.sh 
echo "root: start 0" 
sh 2_setup_vm_boot_source.sh 
echo "root: start 0" 
sh 3_setup_vm_rootfs.sh 
echo "root: start 0" 
sh 4_setup_vm_network.sh 
echo "root: start 0" 
sh 5_start_vm.sh

# ps aux | grep firecracker
# kill -9 111694 112479