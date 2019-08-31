#!/bin/bash

# ARG_OPTIONAL_SINGLE([kfeatures],[f],[Rust features to enable (in the kernel).])
# ARG_OPTIONAL_SINGLE([ufeatures],[u],[Rust features to enable (in user-space).])
# ARG_OPTIONAL_SINGLE([nodes],[a],[How many numa-nodes.],[1])
# ARG_OPTIONAL_SINGLE([cores],[s],[How many cores (evenly divided across NUMA nodes).],[1])
# ARG_OPTIONAL_SINGLE([cmd],[c],[Command line for kernel.])
# ARG_OPTIONAL_REPEATED([mods],[m],[Modules to include on startup.],['init'])
# ARG_OPTIONAL_BOOLEAN([release],[r],[Do a release build.])
# ARG_OPTIONAL_BOOLEAN([norun],[n],[Only build, don't run.])
# ARG_HELP([Bespin runner script])
# ARGBASH_GO()
# [ <-- needed because of Argbash

die()
{
	local _ret=$2
	test -n "$_ret" || _ret=1
	test "$_PRINT_HELP" = yes && print_help >&2
	echo "$1" >&2
	exit ${_ret}
}


begins_with_short_option()
{
	local first_option all_short_options='fuqcmrnh'
	first_option="${1:0:1}"
	test "$all_short_options" = "${all_short_options/$first_option/}" && return 1 || return 0
}

# THE DEFAULTS INITIALIZATION - OPTIONALS
_arg_kfeatures=
_arg_ufeatures=
_arg_qemu=
_arg_cmd=
_arg_mods=('init')
_arg_release="off"
_arg_norun="off"


print_help()
{
	printf '%s\n' "Bespin runner script"
	printf 'Usage: %s [-f|--kfeatures <arg>] [-u|--ufeatures <arg>] [-q|--qemu <arg>] [-c|--cmd <arg>] [-m|--mods <arg>] [-r|--(no-)release] [-n|--(no-)norun] [-h|--help]\n' "$0"
	printf '\t%s\n' "-f, --kfeatures: Rust features to enable (in the kernel). (no default)"
	printf '\t%s\n' "-u, --ufeatures: Rust features to enable (in user-space). (no default)"
	printf '\t%s\n' "-q, --qemu: Optional qemu arguments. (no default)"
	printf '\t%s\n' "-c, --cmd: Command line for kernel. (no default)"
	printf '\t%s' "-m, --mods: Modules to include on startup. (default array elements:"
	printf " '%s'" 'init'
	printf ')\n'
	printf '\t%s\n' "-r, --release, --no-release: Do a release build. (off by default)"
	printf '\t%s\n' "-n, --norun, --no-norun: Only build, don't run. (off by default)"
	printf '\t%s\n' "-h, --help: Prints help"
}


parse_commandline()
{
	while test $# -gt 0
	do
		_key="$1"
		case "$_key" in
			-f|--kfeatures)
				test $# -lt 2 && die "Missing value for the optional argument '$_key'." 1
				_arg_kfeatures="$2"
				shift
				;;
			--kfeatures=*)
				_arg_kfeatures="${_key##--kfeatures=}"
				;;
			-f*)
				_arg_kfeatures="${_key##-f}"
				;;
			-u|--ufeatures)
				test $# -lt 2 && die "Missing value for the optional argument '$_key'." 1
				_arg_ufeatures="$2"
				shift
				;;
			--ufeatures=*)
				_arg_ufeatures="${_key##--ufeatures=}"
				;;
			-u*)
				_arg_ufeatures="${_key##-u}"
				;;
			-q|--qemu)
				test $# -lt 2 && die "Missing value for the optional argument '$_key'." 1
				_arg_qemu="$2"
				shift
				;;
			--qemu=*)
				_arg_qemu="${_key##--qemu=}"
				;;
			-q*)
				_arg_qemu="${_key##-q}"
				;;
			-c|--cmd)
				test $# -lt 2 && die "Missing value for the optional argument '$_key'." 1
				_arg_cmd="$2"
				shift
				;;
			--cmd=*)
				_arg_cmd="${_key##--cmd=}"
				;;
			-c*)
				_arg_cmd="${_key##-c}"
				;;
			-m|--mods)
				test $# -lt 2 && die "Missing value for the optional argument '$_key'." 1
				_arg_mods+=("$2")
				shift
				;;
			--mods=*)
				_arg_mods+=("${_key##--mods=}")
				;;
			-m*)
				_arg_mods+=("${_key##-m}")
				;;
			-r|--no-release|--release)
				_arg_release="on"
				test "${1:0:5}" = "--no-" && _arg_release="off"
				;;
			-r*)
				_arg_release="on"
				_next="${_key##-r}"
				if test -n "$_next" -a "$_next" != "$_key"
				then
					{ begins_with_short_option "$_next" && shift && set -- "-r" "-${_next}" "$@"; } || die "The short option '$_key' can't be decomposed to ${_key:0:2} and -${_key:2}, because ${_key:0:2} doesn't accept value and '-${_key:2:1}' doesn't correspond to a short option."
				fi
				;;
			-n|--no-norun|--norun)
				_arg_norun="on"
				test "${1:0:5}" = "--no-" && _arg_norun="off"
				;;
			-n*)
				_arg_norun="on"
				_next="${_key##-n}"
				if test -n "$_next" -a "$_next" != "$_key"
				then
					{ begins_with_short_option "$_next" && shift && set -- "-n" "-${_next}" "$@"; } || die "The short option '$_key' can't be decomposed to ${_key:0:2} and -${_key:2}, because ${_key:0:2} doesn't accept value and '-${_key:2:1}' doesn't correspond to a short option."
				fi
				;;
			-h|--help)
				print_help
				exit 0
				;;
			-h*)
				print_help
				exit 0
				;;
			*)
				_PRINT_HELP=yes die "FATAL ERROR: Got an unexpected argument '$1'" 1
				;;
		esac
		shift
	done
}

parse_commandline "$@"

# ] <-- needed because of Argbash

set -ex

#
# Building the bootloader
#
echo "> Building the bootloader"
UEFI_TARGET="x86_64-uefi"
if [ "$_arg_release" == "on" ]; then
	UEFI_BUILD_ARGS="--release"
    UEFI_BUILD_DIR="`pwd`/../target/$UEFI_TARGET/release"
	USER_BUILD_ARGS="--release"
	USER_BUILD_DIR="`pwd`/../target/$USER_TARGET/release"
else
	UEFI_BUILD_ARGS=""
	UEFI_BUILD_DIR="`pwd`/../target/$UEFI_TARGET/debug"
	USER_BUILD_ARGS="--features rumprt"
	USER_BUILD_DIR="`pwd`/../target/$USER_TARGET/debug"
fi

if [ "${_arg_ufeatures}" != "" ]; then
    USER_BUILD_ARGS="$BUILD_ARGS --features $_arg_ufeatures"
fi

ESP_DIR=$UEFI_BUILD_DIR/esp

cd ../bootloader
RUST_TARGET_PATH=`pwd` xargo build --target $UEFI_TARGET --package bootloader $UEFI_BUILD_ARGS

QEMU_UEFI_APPEND="-drive if=pflash,format=raw,file=`pwd`/OVMF_CODE.fd,readonly=on"
QEMU_UEFI_APPEND+=" -drive if=pflash,format=raw,file=`pwd`/OVMF_VARS.fd,readonly=on"
QEMU_UEFI_APPEND+=" -device ahci,id=ahci,multifunction=on"
QEMU_UEFI_APPEND+=" -drive if=none,format=raw,file=fat:rw:$ESP_DIR,id=esp"
QEMU_UEFI_APPEND+=" -device ide-drive,bus=ahci.0,drive=esp"

rm -rf $ESP_DIR/EFI
mkdir -p $ESP_DIR/EFI/Boot
cp $UEFI_BUILD_DIR/bootloader.efi $ESP_DIR/EFI/Boot/BootX64.efi

#
# Build user modules
#
echo "> Building user modules"
USER_TARGET="x86_64-bespin-none"
USER_BUILD_ARGS="--verbose --target=$USER_TARGET"

if [ "$_arg_release" == "on" ]; then
	USER_BUILD_ARGS="$USER_BUILD_ARGS --release"
	USER_BUILD_DIR="`pwd`/../target/$USER_TARGET/release"
else
	USER_BUILD_DIR="`pwd`/../target/$USER_TARGET/debug"
fi

if [ "${_arg_ufeatures}" != "" ]; then
    USER_BUILD_ARGS="$USER_BUILD_ARGS --features $_arg_ufeatures"
fi

cd ../usr
if [ "${_arg_mods}" != "" ]; then
    echo "Found MODULES: ${_arg_mods}"
	for item in ${_arg_mods}
	do
		echo "ITEM: $item"
		cd ${item}
		RUST_TARGET_PATH=`pwd`/../ xargo build $USER_BUILD_ARGS
		cp $USER_BUILD_DIR/$item $ESP_DIR/
		cd ..
	done
fi


#
# Building the kernel
#
echo "> Building the kernel"
cd ../kernel
echo "./kernel $_arg_cmd" > cmdline.in

BESPIN_TARGET=x86_64-bespin

export PATH=`pwd`/../binutils-2.30.90/bin:$PATH
if [ -x "$(command -v x86_64-elf-ld)" ] ; then
    # On non-Linux system we should use the cross-compiled linker from binutils
    export CARGO_TARGET_X86_64_BESPIN_LINKER=x86_64-elf-ld
fi

BUILD_ARGS="--target=$BESPIN_TARGET --verbose"

if [ "$_arg_release" == "on" ]; then
    BUILD_ARGS="$BUILD_ARGS --release"
fi

if [ "${_arg_kfeatures}" != "" ]; then
    BUILD_ARGS="$BUILD_ARGS --features $_arg_kfeatures"
fi

BESPIN_TARGET=x86_64-bespin RUST_TARGET_PATH=`pwd`/src/arch/x86_64 xargo build  $BUILD_ARGS

if [ "$_arg_release" == "off" ]; then
    cp ../target/$BESPIN_TARGET/debug/bespin kernel
	cp ../target/$BESPIN_TARGET/debug/bespin $ESP_DIR/kernel
else
    cp ../target/$BESPIN_TARGET/release/bespin kernel
	cp ../target/$BESPIN_TARGET/release/bespin $ESP_DIR/kernel
fi

find $ESP_DIR

#
# Making a bootable image
#
rm -rf uefi.img
dd if=/dev/zero of=uefi.img bs=1k count=65536
mkfs.vfat uefi.img -F 32
mcopy -si uefi.img $ESP_DIR/* ::/

#
# Running things
#
if [ "${_arg_norun}" != "on" ]; then
    set +e
    cat /proc/modules | grep kvm_intel
    if [ $? -eq 0 ]; then
        KVM_ARG='-enable-kvm -cpu host,migratable=no,+invtsc,+tsc' #
    else
		echo "No KVM, system will fail in initializtion since we're missing fs/gs base instructions."
		exit 1
        KVM_ARG='-cpu qemu64'
    fi

    QEMU_NET_APPEND="-net nic,model=e1000,netdev=n0 -netdev tap,id=n0,script=no,ifname=tap0"

	# QEMU Monitor for debug: https://en.wikibooks.org/wiki/QEMU/Monitor
	# qemu-system-x86_64 -d help
	QEMU_MONITOR="-monitor telnet:127.0.0.1:55555,server,nowait -d guest_errors -d int -D debuglog.out"
	#QEMU_MONITOR="-d int,cpu_reset"
	#QEMU_MONITOR="-d cpu_reset,int,guest_errors"

    # Create a tap interface to communicate with guest and give it an IP
    sudo tunctl -t tap0 -u $USER -g `id -gn`
    sudo ifconfig tap0 ip 172.31.0.20/24

	#QEMU_NET_APPEND="-net nic,model=e1000 -net user"
	# -kernel ./mbkernel -initrd kernel
    qemu-system-x86_64 $KVM_ARG -m 1024 -d int -nographic -device isa-debug-exit,iobase=0xf4,iosize=0x04 $QEMU_UEFI_APPEND $QEMU_NET_APPEND $CMDLINE_APPEND $QEMU_MONITOR ${_arg_qemu}
    QEMU_EXIT=$?
    set +ex
    # qemu will do exit((val << 1) | 1);
    BESPIN_EXIT=$(($QEMU_EXIT >> 1))
    case "$BESPIN_EXIT" in
        0)
        MESSAGE="[SUCCESS]"
        ;;
        1)
        MESSAGE="[FAIL] ReturnFromMain: main() function returned to arch_indepdendent part."
        ;;
        2)
        MESSAGE="[FAIL] Encountered kernel panic."
        ;;
        3)
        MESSAGE="[FAIL] Encountered OOM."
        ;;
        4)
        MESSAGE="[FAIL] Encountered unexpected Interrupt."
        ;;
        5)
        MESSAGE="[FAIL] General Protection Fault."
        ;;
        6)
        MESSAGE="[FAIL] Unexpected Page Fault."
        ;;
        7)
        MESSAGE="[FAIL] Unexpected process exit code when running a user-space test."
        ;;
        *)
        MESSAGE="[FAIL] Kernel exited with unknown error status $BESPIN_EXIT... Update the script!"
        ;;
    esac
    echo $MESSAGE
    exit $BESPIN_EXIT
fi
