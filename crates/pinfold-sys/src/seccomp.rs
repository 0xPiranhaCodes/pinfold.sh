//! The seccomp filter bubblewrap puts a sandboxed agent under on Linux (ADR-0020 point 5,
//! ADR-0037): `ioctl(_, TIOCSTI, _)` fails with `EPERM`, and so does every x32 call on x86_64.
//! A call made under another architecture's numbering, such as i386's, kills the process.
//! Everything else is allowed.

use std::io;
use std::os::fd::OwnedFd;

/// Returns a pipe holding the filter as classic BPF, for bubblewrap's `--seccomp`, on fd 3 or
/// above and close-on-exec. bwrap reads it to the end, so each launch needs its own.
///
/// # Errors
///
/// Returns an error if the pipe cannot be made, or if this is not Linux on x86_64 or
/// little-endian aarch64, the architectures the filter has numbers for.
///
/// # Examples
///
/// ```
/// match pinfold_sys::seccomp::filter() {
///     Ok(filter) => assert!(std::os::fd::AsRawFd::as_raw_fd(&filter) >= 3),
///     Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::Unsupported),
/// }
/// ```
pub fn filter() -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    {
        linux::filter()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{self, Write};
    use std::mem::offset_of;
    use std::os::fd::{AsRawFd, OwnedFd};

    use libc::{
        BPF_ABS, BPF_JEQ, BPF_JGE, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, seccomp_data,
        sock_filter,
    };

    // AUDIT_ARCH_* from linux/audit.h: the ELF machine, marked 64-bit and little-endian.
    #[cfg(target_arch = "x86_64")]
    const ARCH: Option<u32> = Some(0xC000_003E);
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    const ARCH: Option<u32> = Some(0xC000_00B7);
    #[cfg(not(any(
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_endian = "little")
    )))]
    const ARCH: Option<u32> = None;

    /// x86_64's x32 calls are numbered from here, and are refused rather than checked.
    const X32: u32 = 0x4000_0000;

    pub(super) fn filter() -> io::Result<OwnedFd> {
        let arch = ARCH.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no seccomp filter for this architecture, so the sandboxed agent does not start",
            )
        })?;
        let mut bytes = Vec::new();
        for op in program(arch) {
            bytes.extend(op.code.to_ne_bytes());
            bytes.extend([op.jt, op.jf]);
            bytes.extend(op.k.to_ne_bytes());
        }
        let (reader, mut writer) = io::pipe()?;
        // 80 bytes fit in any pipe's buffer, so this does not block.
        writer.write_all(&bytes)?;
        // Not 0 to 2, where the child's stdio would land over it.
        crate::socket::duplicate(reader.as_raw_fd(), 3)
    }

    /// The program. Another architecture's calls are numbered differently, so it is killed.
    fn program(arch: u32) -> [sock_filter; 10] {
        let load = |offset: usize| op(BPF_LD | BPF_W | BPF_ABS, offset as u32, 0, 0);
        let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
        [
            load(offset_of!(seccomp_data, arch)),
            op(BPF_JMP | BPF_JEQ | BPF_K, arch, 1, 0),
            op(BPF_RET | BPF_K, libc::SECCOMP_RET_KILL_PROCESS, 0, 0),
            load(offset_of!(seccomp_data, nr)),
            op(BPF_JMP | BPF_JGE | BPF_K, X32, 4, 0),
            op(BPF_JMP | BPF_JEQ | BPF_K, libc::SYS_ioctl as u32, 0, 2),
            // The kernel reads the request as 32 bits, so only the low, first half counts.
            load(offset_of!(seccomp_data, args) + 8),
            op(BPF_JMP | BPF_JEQ | BPF_K, libc::TIOCSTI as u32, 1, 0),
            op(BPF_RET | BPF_K, libc::SECCOMP_RET_ALLOW, 0, 0),
            op(BPF_RET | BPF_K, deny, 0, 0),
        ]
    }

    fn op(code: u32, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    #[cfg(test)]
    mod tests {
        use std::io::Read;

        use super::*;

        /// Runs `filter` on a call as the kernel would, with `request` as the second argument.
        fn run(filter: &[sock_filter], arch: u32, nr: u32, request: u64) -> u32 {
            // seccomp_data as little-endian words: nr, arch, the pointer, then the arguments.
            let mut words = [0; 16];
            (words[0], words[1]) = (nr, arch);
            (words[6], words[7]) = (request as u32, (request >> 32) as u32);
            let (mut pc, mut a) = (0, 0);
            loop {
                let op = filter[pc];
                pc += 1;
                let taken = |test: bool| usize::from(if test { op.jt } else { op.jf });
                match u32::from(op.code) {
                    code if code == BPF_LD | BPF_W | BPF_ABS => a = words[op.k as usize / 4],
                    code if code == BPF_JMP | BPF_JEQ | BPF_K => pc += taken(a == op.k),
                    code if code == BPF_JMP | BPF_JGE | BPF_K => pc += taken(a >= op.k),
                    code if code == BPF_RET | BPF_K => return op.k,
                    code => panic!("unexpected op {code:#x}"),
                }
            }
        }

        #[test]
        fn only_tiocsti_is_denied() {
            let arch = ARCH.unwrap();
            let filter = program(arch);
            let (ioctl, write) = (libc::SYS_ioctl as u32, libc::SYS_write as u32);
            let (tiocsti, winsize) = (libc::TIOCSTI as u32, libc::TIOCGWINSZ as u32);
            let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
            assert_eq!(run(&filter, arch, ioctl, tiocsti.into()), deny);
            assert_eq!(
                run(&filter, arch, ioctl, u64::from(tiocsti) | 1 << 32),
                deny
            );
            assert_eq!(run(&filter, arch, ioctl | X32, tiocsti.into()), deny);
            let allow = libc::SECCOMP_RET_ALLOW;
            assert_eq!(run(&filter, arch, ioctl, winsize.into()), allow);
            assert_eq!(run(&filter, arch, write, tiocsti.into()), allow);
            let kill = libc::SECCOMP_RET_KILL_PROCESS;
            assert_eq!(run(&filter, arch ^ 1, ioctl, winsize.into()), kill);
        }

        #[test]
        fn the_pipe_holds_the_whole_program() {
            let mut bytes = Vec::new();
            std::fs::File::from(filter().unwrap())
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes.len(), 10 * size_of::<sock_filter>());
        }
    }
}
