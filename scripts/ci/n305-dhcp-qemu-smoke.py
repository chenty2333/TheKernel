#!/usr/bin/env python3
"""No boot Slirp configuration on N305; real DHCP and no-router lease change."""
from pathlib import Path
import os
import re
import subprocess
import sys
import tempfile
ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT))
from tools.product_state import state_root
COMMANDS='''echo NET_BEFORE_BEGIN
ip -4 addr show dev eth0
ip -4 route show
echo NET_BEFORE_END
udhcpc -i eth0 -n -q -t 4 -T 2 -s /etc/thekernel/n305-dhcp.script
echo NET_DHCP_BEGIN
ip -4 addr show dev eth0
ip -4 route show
ping -c 2 10.0.2.2
echo NET_DHCP_END
interface=eth0 ip=192.168.10.15 subnet=255.255.255.0 router= /etc/thekernel/n305-dhcp.script renew
echo NET_NO_ROUTER_BEGIN
ip -4 addr show dev eth0
ip -4 route show
echo NET_NO_ROUTER_END
poweroff -f
'''
def validate(console: str):
    console=console.replace('\r','')
    def section(name):
        found=re.search(r'(?ms)^NET_'+name+r'_BEGIN\n(.*?)^NET_'+name+r'_END$',console)
        if found is None:raise RuntimeError('missing guest section '+name)
        return found.group(1)
    before=section('BEFORE');leased=section('DHCP');none=section('NO_ROUTER')
    if 'inet ' in before or 'default ' in before or '10.0.2.' in before:
        raise RuntimeError('N305 started with a synthetic Slirp address or route')
    if 'inet 10.0.2.15/24' not in leased or 'default via 10.0.2.2 dev eth0' not in leased or '2 packets received' not in leased:
        raise RuntimeError('actual DHCP/traffic did not work from an unconfigured interface')
    if 'inet 192.168.10.15/24' not in none or 'default ' in none or '10.0.2.' in none:
        raise RuntimeError('no-router lease retained a default or old address/subnet route')

def main():
    runs=state_root()/'runs';runs.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='dhcp-',dir=runs) as directory:
        directory=Path(directory);commands=directory/'commands';commands.write_text(COMMANDS);work=directory/'run'
        subprocess.run([sys.executable,str(ROOT/'tools/thekernel.py'),'run','--platform','n305','--profile','shell',
                        '--accel','kvm','--commands',str(commands),'--workdir',str(work),'--timeout','150'],cwd=ROOT,env=os.environ,check=True)
        validate((work/'console.log').read_text());print('N305_DHCP_QEMU_PASS')
if __name__=='__main__':main()
