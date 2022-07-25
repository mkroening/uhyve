use goblin::elf;
use goblin::elf64::header::ET_DYN;
use goblin::elf64::program_header::{PT_LOAD, PT_TLS};
use goblin::elf64::reloc::*;
use hermit_entry::{BootInfo, NetInfo, RawBootInfo, TlsInfo};
use log::{debug, error, warn};
use std::ffi::OsString;
use std::io::Write;
use std::net::Ipv4Addr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::SystemTime;
use std::{fs, io, mem, slice};
use thiserror::Error;

#[cfg(target_arch = "x86_64")]
use crate::arch::x86_64::{
	detect_freq_from_cpuid, detect_freq_from_cpuid_hypervisor_info, get_cpu_frequency_from_os,
	ELF_HOST_ARCH,
};

#[cfg(target_arch = "aarch64")]
use crate::arch::aarch64::ELF_HOST_ARCH;

use crate::consts::*;
use crate::os::vcpu::UhyveCPU;
use crate::os::DebugExitInfo;
use crate::os::HypervisorError;

#[repr(C, packed)]
pub struct SysWrite {
	fd: i32,
	buf: *const u8,
	len: usize,
}

#[repr(C, packed)]
pub struct SysRead {
	fd: i32,
	buf: *const u8,
	len: usize,
	ret: isize,
}

#[repr(C, packed)]
pub struct SysClose {
	fd: i32,
	ret: i32,
}

#[repr(C, packed)]
pub struct SysOpen {
	name: *const u8,
	flags: i32,
	mode: i32,
	ret: i32,
}

#[repr(C, packed)]
pub struct SysLseek {
	fd: i32,
	offset: isize,
	whence: i32,
}

#[repr(C, packed)]
pub struct SysExit {
	arg: i32,
}

// FIXME: Do not use a fix number of arguments
const MAX_ARGC: usize = 128;
// FIXME: Do not use a fix number of environment variables
const MAX_ENVC: usize = 128;

#[repr(C, packed)]
pub struct SysCmdsize {
	argc: i32,
	argsz: [i32; MAX_ARGC],
	envc: i32,
	envsz: [i32; MAX_ENVC],
}

#[repr(C, packed)]
pub struct SysCmdval {
	argv: *const u8,
	envp: *const u8,
}

#[repr(C, packed)]
pub struct SysUnlink {
	name: *const u8,
	ret: i32,
}

pub type HypervisorResult<T> = Result<T, HypervisorError>;

#[derive(Error, Debug)]
pub enum LoadKernelError {
	#[error(transparent)]
	Io(#[from] io::Error),
	#[error(transparent)]
	Goblin(#[from] goblin::error::Error),
	#[error("guest memory size is not large enough")]
	InsufficientMemory,
}

pub type LoadKernelResult<T> = Result<T, LoadKernelError>;

/// Reasons for vCPU exits.
pub enum VcpuStopReason {
	/// The vCPU stopped for debugging.
	Debug(DebugExitInfo),

	/// The vCPU exited with the specified exit code.
	Exit(i32),

	/// The vCPU got kicked.
	Kick,
}

pub trait VirtualCPU {
	/// Initialize the cpu to start running the code ad entry_point.
	fn init(&mut self, entry_point: u64, cpu_id: u32) -> HypervisorResult<()>;

	/// Continues execution.
	fn r#continue(&mut self) -> HypervisorResult<VcpuStopReason>;

	/// Start the execution of the CPU. The function will run until it crashes (`Err`) or terminate with an exit code (`Ok`).
	fn run(&mut self) -> HypervisorResult<Option<i32>>;

	/// Prints the VCPU's registers to stdout.
	fn print_registers(&self);

	/// Translates an address from the VM's physical space into the hosts virtual space.
	fn host_address(&self, addr: usize) -> usize;

	/// Looks up the guests pagetable and translates a guest's virtual address to a guest's physical address.
	fn virt_to_phys(&self, addr: usize) -> usize;

	/// Returns the (host) path of the kernel binary.
	fn kernel_path(&self) -> &Path;

	fn args(&self) -> &[OsString];

	fn cmdsize(&self, syssize: &mut SysCmdsize) {
		syssize.argc = 0;
		syssize.envc = 0;

		let path = self.kernel_path();
		syssize.argsz[0] = path.as_os_str().len() as i32 + 1;

		let mut counter = 0;
		for argument in self.args() {
			syssize.argsz[(counter + 1) as usize] = argument.len() as i32 + 1;

			counter += 1;
		}

		syssize.argc = counter + 1;

		let mut counter = 0;
		for (key, value) in std::env::vars_os() {
			if counter < MAX_ENVC.try_into().unwrap() {
				syssize.envsz[counter as usize] = (key.len() + value.len()) as i32 + 2;
				counter += 1;
			}
		}
		syssize.envc = counter;

		if counter >= MAX_ENVC.try_into().unwrap() {
			warn!("Environment is too large!");
		}
	}

	/// Copies the arguments end environment of the application into the VM's memory.
	fn cmdval(&self, syscmdval: &SysCmdval) {
		let argv = self.host_address(syscmdval.argv as usize);

		// copy kernel path as first argument
		{
			let path = self.kernel_path().as_os_str();

			let argvptr = unsafe { self.host_address(*(argv as *mut *mut u8) as usize) };
			let len = path.len();
			let slice = unsafe { slice::from_raw_parts_mut(argvptr as *mut u8, len + 1) };

			// Create string for environment variable
			slice[0..len].copy_from_slice(path.as_bytes());
			slice[len] = 0;
		}

		// Copy the application arguments into the vm memory
		for (counter, argument) in self.args().iter().enumerate() {
			let argvptr = unsafe {
				self.host_address(
					*((argv + (counter + 1) as usize * mem::size_of::<usize>()) as *mut *mut u8)
						as usize,
				)
			};
			let len = argument.len();
			let slice = unsafe { slice::from_raw_parts_mut(argvptr as *mut u8, len + 1) };

			// Create string for environment variable
			slice[0..len].copy_from_slice(argument.as_bytes());
			slice[len] = 0;
		}

		// Copy the environment variables into the vm memory
		let mut counter = 0;
		let envp = self.host_address(syscmdval.envp as usize);
		for (key, value) in std::env::vars_os() {
			if counter < MAX_ENVC.try_into().unwrap() {
				let envptr = unsafe {
					self.host_address(
						*((envp + counter as usize * mem::size_of::<usize>()) as *mut *mut u8)
							as usize,
					)
				};
				let len = key.len() + value.len();
				let slice = unsafe { slice::from_raw_parts_mut(envptr as *mut u8, len + 2) };

				// Create string for environment variable
				slice[0..key.len()].copy_from_slice(key.as_bytes());
				slice[key.len()..(key.len() + 1)].copy_from_slice("=".as_bytes());
				slice[(key.len() + 1)..(len + 1)].copy_from_slice(value.as_bytes());
				slice[len + 1] = 0;
				counter += 1;
			}
		}
	}

	/// unlink deletes a name from the filesystem. This is used to handle `unlink` syscalls from the guest.
	/// TODO: UNSAFE AS *%@#. It has to be checked that the VM is allowed to unlink that file!
	fn unlink(&self, sysunlink: &mut SysUnlink) {
		unsafe {
			sysunlink.ret = libc::unlink(self.host_address(sysunlink.name as usize) as *const i8);
		}
	}

	/// Reads the exit code from an VM and returns it
	fn exit(&self, sysexit: &SysExit) -> i32 {
		sysexit.arg
	}

	/// Handles an open syscall by opening a file on the host.
	fn open(&self, sysopen: &mut SysOpen) {
		unsafe {
			sysopen.ret = libc::open(
				self.host_address(sysopen.name as usize) as *const i8,
				sysopen.flags,
				sysopen.mode,
			);
		}
	}

	/// Handles an close syscall by closing the file on the host.
	fn close(&self, sysclose: &mut SysClose) {
		unsafe {
			sysclose.ret = libc::close(sysclose.fd);
		}
	}

	/// Handles an read syscall on the host.
	fn read(&self, sysread: &mut SysRead) {
		unsafe {
			let buffer = self.virt_to_phys(sysread.buf as usize);

			let bytes_read = libc::read(
				sysread.fd,
				self.host_address(buffer) as *mut libc::c_void,
				sysread.len,
			);
			if bytes_read >= 0 {
				sysread.ret = bytes_read;
			} else {
				sysread.ret = -1;
			}
		}
	}

	/// Handles an write syscall on the host.
	fn write(&self, syswrite: &SysWrite) -> io::Result<()> {
		let mut bytes_written: usize = 0;
		let buffer = self.virt_to_phys(syswrite.buf as usize);

		while bytes_written != syswrite.len {
			unsafe {
				let step = libc::write(
					syswrite.fd,
					self.host_address(buffer + bytes_written) as *const libc::c_void,
					syswrite.len - bytes_written,
				);
				if step >= 0 {
					bytes_written += step as usize;
				} else {
					return Err(io::Error::last_os_error());
				}
			}
		}

		Ok(())
	}

	/// Handles an write syscall on the host.
	fn lseek(&self, syslseek: &mut SysLseek) {
		unsafe {
			syslseek.offset =
				libc::lseek(syslseek.fd, syslseek.offset as i64, syslseek.whence) as isize;
		}
	}

	/// Handles an UART syscall by writing to stdout.
	fn uart(&self, buf: &[u8]) -> io::Result<()> {
		io::stdout().write_all(buf)
	}
}

pub trait Vm {
	/// Returns the number of cores for the vm.
	fn num_cpus(&self) -> u32;
	/// Returns a pointer to the address of the guest memory and the size of the memory in bytes.
	fn guest_mem(&self) -> (*mut u8, usize);
	#[doc(hidden)]
	fn set_offset(&mut self, offset: u64);
	/// Returns the section offsets relative to their base addresses
	fn get_offset(&self) -> u64;
	/// Sets the elf entry point.
	fn set_entry_point(&mut self, entry: u64);
	fn get_entry_point(&self) -> u64;
	fn kernel_path(&self) -> &Path;
	fn create_cpu(&self, id: u32) -> HypervisorResult<UhyveCPU>;
	fn set_boot_info(&mut self, header: *const RawBootInfo);
	fn cpu_online(&self) -> u32;
	fn get_ip(&self) -> Option<Ipv4Addr>;
	fn get_gateway(&self) -> Option<Ipv4Addr>;
	fn get_mask(&self) -> Option<Ipv4Addr>;
	fn verbose(&self) -> bool;
	fn init_guest_mem(&self);

	unsafe fn load_kernel(&mut self) -> LoadKernelResult<()> {
		debug!("Load kernel from {}", self.kernel_path().display());

		let buffer = fs::read(self.kernel_path())?;
		let elf = elf::Elf::parse(&buffer)?;

		if !elf.libraries.is_empty() {
			warn!(
				"Error: file depends on following libraries: {:?}",
				elf.libraries
			);
			return Err(LoadKernelError::Io(io::ErrorKind::InvalidData.into()));
		}

		let is_dyn = elf.header.e_type == ET_DYN;
		debug!("ELF file is a shared object file: {}", is_dyn);

		if elf.header.e_machine != ELF_HOST_ARCH {
			return Err(LoadKernelError::Io(io::ErrorKind::InvalidData.into()));
		}

		if let Some(mut note_headers) = elf.iter_note_headers(&buffer) {
			if let Some(note) = note_headers.find(|note| {
				note.as_ref().unwrap().name == "HERMIT"
					&& note.as_ref().unwrap().n_type == hermit_entry::NT_HERMIT_ENTRY_VERSION
			}) {
				let expected = 1;
				let found = note.unwrap().desc[0];
				if found != expected {
					error!("Expected hermit entry version {expected}, found {found}");
					return Err(LoadKernelError::Io(io::ErrorKind::InvalidData.into()));
				}
			} else {
				error!("Kernel does not specify hermit entry version! - This might lead to undefined behaviour and will be deprecated in the future.");
			}
		} else {
			error!("Kernel elf does not contain notes section to specify hermit entry version! - This might lead to undefined behaviour and will be deprecated in the future.");
		}

		// acquire the slices of the user memory
		let (vm_mem, vm_mem_length) = self.guest_mem();

		// Collect BootInfo, starting with NetInfo

		let mut net_info = NetInfo::default();

		// forward IP address to kernel
		if let Some(ip) = self.get_ip() {
			net_info.ip = ip.octets();
		}

		// forward gateway address to kernel
		if let Some(gateway) = self.get_gateway() {
			net_info.gateway = gateway.octets();
		}

		// forward mask to kernel
		if let Some(mask) = self.get_mask() {
			net_info.mask = mask.octets();
		}

		let (start_address, elf_entry) = if is_dyn {
			// TODO: should be a random start address, if we have a relocatable executable
			(0x400000u64, 0x400000u64 + elf.entry)
		} else {
			// default location of a non-relocatable binary
			(0x800000u64, elf.entry)
		};

		self.set_offset(start_address);
		self.set_entry_point(elf_entry);
		debug!("ELF entry point at 0x{:x}", elf_entry);

		let n = SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.expect("SystemTime before UNIX EPOCH!");

		#[cfg(target_arch = "aarch64")]
		let mhz: u32 = 0;
		#[cfg(target_arch = "x86_64")]
		let mhz = {
			let cpuid = raw_cpuid::CpuId::new();
			let mhz: u32 = detect_freq_from_cpuid(&cpuid).unwrap_or_else(|_| {
				debug!("Failed to detect from cpuid");
				detect_freq_from_cpuid_hypervisor_info(&cpuid).unwrap_or_else(|_| {
					debug!("Failed to detect from hypervisor_info");
					get_cpu_frequency_from_os().unwrap_or(0)
				})
			});
			debug!("detected a cpu frequency of {} Mhz", mhz);

			mhz
		};
		if mhz == 0 {
			warn!("Unable to determine processor frequency");
		}

		// load kernel and determine image size
		let vm_slice = std::slice::from_raw_parts_mut(vm_mem, vm_mem_length);
		let mut image_size = 0;
		let mut tls_info = TlsInfo::default();
		elf.program_headers
			.iter()
			.try_for_each(|program_header| match program_header.p_type {
				PT_LOAD => {
					let region_start = if is_dyn {
						(start_address + program_header.p_vaddr) as usize
					} else {
						program_header.p_vaddr as usize
					};
					let region_end = region_start + program_header.p_filesz as usize;
					let kernel_start = program_header.p_offset as usize;
					let kernel_end = kernel_start + program_header.p_filesz as usize;

					debug!(
						"Load segment with start addr 0x{:x} and size 0x{:x}, offset 0x{:x}",
						program_header.p_vaddr, program_header.p_filesz, program_header.p_offset
					);

					if region_start + program_header.p_memsz as usize > vm_mem_length {
						return Err(LoadKernelError::InsufficientMemory);
					}

					vm_slice[region_start..region_end]
						.copy_from_slice(&buffer[kernel_start..kernel_end]);

					if program_header.p_memsz > program_header.p_filesz {
						vm_slice[region_end
							..region_end
								+ (program_header.p_memsz - program_header.p_filesz) as usize]
							.iter_mut()
							.for_each(|x| *x = 0);
					}

					image_size = if is_dyn {
						program_header.p_vaddr + program_header.p_memsz
					} else {
						image_size + program_header.p_memsz
					};

					Ok(())
				}
				PT_TLS => {
					// determine TLS section
					debug!("Found TLS section with size {}", program_header.p_memsz);
					let tls_start = if is_dyn {
						start_address + program_header.p_vaddr
					} else {
						program_header.p_vaddr
					};

					tls_info = TlsInfo {
						start: tls_start,
						filesz: program_header.p_filesz,
						memsz: program_header.p_memsz,
						align: program_header.p_align,
					};

					Ok(())
				}
				_ => Ok(()),
			})?;

		// relocate entries (strings, copy-data, etc.) with an addend
		elf.dynrelas.iter().for_each(|rela| match rela.r_type {
			R_X86_64_RELATIVE | R_AARCH64_RELATIVE => {
				let offset = (vm_mem as u64 + start_address + rela.r_offset) as *mut u64;
				*offset = (start_address as i64 + rela.r_addend.unwrap_or(0))
					.try_into()
					.unwrap();
			}
			_ => {
				debug!("Unsupported relocation type {}", rela.r_type);
			}
		});

		elf.dynrels.iter().for_each(|rel| {
			debug!("rel {:?}", rel);
		});

		let boot_info = BootInfo {
			base: start_address,
			limit: vm_mem_length as u64, // memory size
			image_size,
			tls_info,
			current_stack_address: start_address - KERNEL_STACK_SIZE,
			host_logical_addr: vm_mem as u64,
			boot_gtod: n.as_micros().try_into().unwrap(),
			cpu_freq: mhz,
			possible_cpus: 1,
			uartport: self
				.verbose()
				.then(|| UHYVE_UART_PORT.into())
				.unwrap_or_default(),
			uhyve: if cfg!(target_os = "linux") {
				0b11 // announce uhyve and pci support
			} else {
				0b01 // announce uhyve
			},
			net_info,
			#[cfg(target_arch = "aarch64")]
			ram_start: crate::arch::aarch64::RAM_START,
			..Default::default()
		};

		debug!("Boot header: {:?}", boot_info);
		let raw_boot_info = vm_mem.offset(BOOT_INFO_ADDR as isize) as *mut RawBootInfo;
		*raw_boot_info = boot_info.into();
		debug!("Set HermitCore header at 0x{:x}", BOOT_INFO_ADDR as usize);
		self.set_boot_info(raw_boot_info);

		debug!("Kernel loaded");

		Ok(())
	}
}
