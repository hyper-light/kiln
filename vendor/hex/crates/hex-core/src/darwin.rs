//! What macOS numbers apart from Linux, where the seam numbers it as Linux does: signals. A
//! [`hex_api::Signal`] carries Linux's number, x86_64's and aarch64's alike (asm-generic's,
//! `include/uapi/asm-generic/signal.h`), and so does an [`hex_api::Exit::Signal`] from a box on any OS,
//! so one caller reads every OS's boxes alike (CLAUDE.md §8). macOS's numbers are XNU's
//! (`bsd/sys/signal.h:84-123`, xnu-12377.1.9).

/// Each signal both kernels have: its name, Linux's number and macOS's. Linux's SIGSTKFLT (16), SIGPWR
/// (30) and real-time signals (32 to 64) have no macOS number, nor macOS's SIGEMT (7) and SIGINFO (29) a
/// Linux one.
const SIGNALS: [(&str, i32, i32); 29] = [
    ("HUP", 1, 1),
    ("INT", 2, 2),
    ("QUIT", 3, 3),
    ("ILL", 4, 4),
    ("TRAP", 5, 5),
    ("ABRT", 6, 6),
    ("BUS", 7, 10),
    ("FPE", 8, 8),
    ("KILL", 9, 9),
    ("USR1", 10, 30),
    ("SEGV", 11, 11),
    ("USR2", 12, 31),
    ("PIPE", 13, 13),
    ("ALRM", 14, 14),
    ("TERM", 15, 15),
    ("CHLD", 17, 20),
    ("CONT", 18, 19),
    ("STOP", 19, 17),
    ("TSTP", 20, 18),
    ("TTIN", 21, 21),
    ("TTOU", 22, 22),
    ("URG", 23, 16),
    ("XCPU", 24, 24),
    ("XFSZ", 25, 25),
    ("VTALRM", 26, 26),
    ("PROF", 27, 27),
    ("WINCH", 28, 28),
    ("IO", 29, 23),
    ("SYS", 31, 12),
];

/// macOS's number for Linux's signal `linux`, if macOS has the signal.
pub fn signal_to_macos(linux: i32) -> Option<i32> {
    SIGNALS
        .iter()
        .find(|(_, l, _)| *l == linux)
        .map(|(_, _, m)| *m)
}

/// Linux's number for macOS's signal `macos`, if Linux has the signal.
pub fn signal_from_macos(macos: i32) -> Option<i32> {
    SIGNALS
        .iter()
        .find(|(_, _, m)| *m == macos)
        .map(|(_, l, _)| *l)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each signal goes to macOS and back as itself, and no two share a number on either side.
    #[test]
    fn every_signal_both_kernels_have_goes_to_macos_and_back() {
        for (name, linux, macos) in SIGNALS {
            assert_eq!(signal_to_macos(linux), Some(macos), "{name}");
            assert_eq!(signal_from_macos(macos), Some(linux), "{name}");
        }
        let mut linux: Vec<i32> = SIGNALS.iter().map(|s| s.1).collect();
        let mut macos: Vec<i32> = SIGNALS.iter().map(|s| s.2).collect();
        linux.sort_unstable();
        linux.dedup();
        macos.sort_unstable();
        macos.dedup();
        assert_eq!((linux.len(), macos.len()), (SIGNALS.len(), SIGNALS.len()));
    }

    /// A signal only one kernel has has no number on the other: Linux's SIGSTKFLT, SIGPWR and real-time
    /// signals, macOS's SIGEMT and SIGINFO.
    #[test]
    fn a_signal_one_kernel_lacks_has_no_number_there() {
        for linux in [16, 30, 32, 64] {
            assert_eq!(signal_to_macos(linux), None, "Linux {linux}");
        }
        for macos in [7, 29] {
            assert_eq!(signal_from_macos(macos), None, "macOS {macos}");
        }
        assert_eq!(signal_to_macos(0), None);
    }

    /// Doc-truth (CLAUDE.md §6): the table's Linux numbers are this Linux's own.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_numbers_are_the_kernels() {
        let own = [
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGILL,
            libc::SIGTRAP,
            libc::SIGABRT,
            libc::SIGBUS,
            libc::SIGFPE,
            libc::SIGKILL,
            libc::SIGUSR1,
            libc::SIGSEGV,
            libc::SIGUSR2,
            libc::SIGPIPE,
            libc::SIGALRM,
            libc::SIGTERM,
            libc::SIGCHLD,
            libc::SIGCONT,
            libc::SIGSTOP,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGURG,
            libc::SIGXCPU,
            libc::SIGXFSZ,
            libc::SIGVTALRM,
            libc::SIGPROF,
            libc::SIGWINCH,
            libc::SIGIO,
            libc::SIGSYS,
        ];
        for ((name, linux, _), own) in SIGNALS.iter().zip(own) {
            assert_eq!(*linux, own, "SIG{name}");
        }
    }

    /// Doc-truth (CLAUDE.md §6): the table's macOS numbers are this macOS's own.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_numbers_are_the_kernels() {
        let own = [
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGILL,
            libc::SIGTRAP,
            libc::SIGABRT,
            libc::SIGBUS,
            libc::SIGFPE,
            libc::SIGKILL,
            libc::SIGUSR1,
            libc::SIGSEGV,
            libc::SIGUSR2,
            libc::SIGPIPE,
            libc::SIGALRM,
            libc::SIGTERM,
            libc::SIGCHLD,
            libc::SIGCONT,
            libc::SIGSTOP,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGURG,
            libc::SIGXCPU,
            libc::SIGXFSZ,
            libc::SIGVTALRM,
            libc::SIGPROF,
            libc::SIGWINCH,
            libc::SIGIO,
            libc::SIGSYS,
        ];
        for ((name, _, macos), own) in SIGNALS.iter().zip(own) {
            assert_eq!(*macos, own, "SIG{name}");
        }
    }
}
