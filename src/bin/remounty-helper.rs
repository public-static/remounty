//! Remounty's privileged helper. Installed root-owned in
//! /Library/PrivilegedHelperTools/remounty and run through sudo; see
//! `remounty::helper` for what it does and how it protects itself.

fn main() -> std::process::ExitCode {
    remounty::helper::main()
}
