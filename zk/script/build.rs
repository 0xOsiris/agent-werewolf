//! Builds the guest ELF that `include_elf!` embeds.

fn main() {
    sp1_build::build_program_with_args(
        "../program",
        sp1_build::BuildArgs {
            locked: true,
            warning_level: sp1_build::WarningLevel::Minimal,
            ..Default::default()
        },
    );
}
