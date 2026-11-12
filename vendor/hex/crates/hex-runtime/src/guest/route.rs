//! Resolve a committed host route without probing a rootfs or choosing a device.

use hex_api::{BoxSpec, Class, Execution, ImageArch, ImageOs, ImagePlatform, Refusal};

use super::Side;

pub(super) fn invalid(why: &'static str) -> Refusal {
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return Refusal::Exhausted {
            what: "guest route refusal",
            bound: why.len(),
        };
    }
    text.push_str(why);
    Refusal::Invalid {
        what: "guest execution route",
        why: text,
    }
}

pub(super) fn host_platform() -> Result<ImagePlatform, Refusal> {
    let os = match std::env::consts::OS {
        "linux" => ImageOs::Linux,
        "macos" => ImageOs::Macos,
        "windows" => ImageOs::Windows,
        _ => return Err(invalid("host image platform is not supported")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => ImageArch::Amd64,
        "aarch64" => ImageArch::Arm64,
        _ => return Err(invalid("host image architecture is not supported")),
    };
    if os == ImageOs::Macos && arch != ImageArch::Arm64 {
        return Err(invalid("the macOS backend requires arm64"));
    }
    Ok(ImagePlatform { os, arch })
}

/// `None` retains the old class router only where the caller can establish an
/// unambiguous legacy path. Rootfs representation supplies no image OS evidence.
pub(super) fn committed_side(spec: &BoxSpec, host: ImagePlatform) -> Result<Option<Side>, Refusal> {
    spec.check()?;
    let Some(receipt) = spec.route else {
        return Ok(None);
    };
    match receipt.key.execution {
        Execution::Native | Execution::Firecracker => {
            if receipt.key.execution == Execution::Native && receipt.key.image != host {
                return Err(invalid(
                    "native execution differs from the committed image platform",
                ));
            }
            if receipt.key.execution == Execution::Firecracker
                && (host.os != ImageOs::Linux || receipt.key.image.arch != host.arch)
            {
                return Err(invalid(
                    "Firecracker requires its matching Linux native backend",
                ));
            }
            Ok(Some(Side::Host))
        }
        Execution::ShardsShared | Execution::ShardsDedicated => {
            if receipt.key.image.os != ImageOs::Linux || receipt.key.image.arch != host.arch {
                return Err(invalid(
                    "Shards executes a matching-architecture Linux image",
                ));
            }
            if receipt.key.execution == Execution::ShardsShared && spec.allocation.is_some() {
                return Err(invalid(
                    "shared inner work cannot mint a per-box VM allocation",
                ));
            }
            Ok(Some(Side::Guest))
        }
    }
}

pub(super) fn guest_spec(spec: &BoxSpec) -> Result<(), Refusal> {
    if committed_side(spec, host_platform()?)?.is_some_and(|side| side != Side::Guest) {
        return Err(invalid(
            "the committed execution belongs to another backend",
        ));
    }
    if !matches!(spec.class.class(), Class::Oci | Class::Shards) {
        return Err(invalid(
            "the Shards guest requires an OCI or Shards workload",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{BackingKey, GpuEligibility, RouteKey, RouteReceipt};

    fn host(os: ImageOs, arch: ImageArch) -> ImagePlatform {
        ImagePlatform { os, arch }
    }

    fn routed(os: ImageOs, execution: Execution) -> BoxSpec {
        let mut spec = super::super::tests::oci(hex_api::Rootfs::Layers(Vec::new()), Vec::new());
        // Valid layer content is independent of the platform/route oracle.
        let hex_api::ClassSpec::Oci(oci) = &mut spec.class else {
            panic!("OCI fixture")
        };
        oci.rootfs =
            hex_api::Rootfs::Layers(vec![std::env::current_dir().unwrap().join("layer.tar")]);
        spec.route = Some(RouteReceipt {
            key: RouteKey {
                class: Class::Oci,
                image: host(os, ImageArch::Arm64),
                execution,
                gpu: GpuEligibility::None,
            },
            backing: if execution == Execution::ShardsShared {
                Some(BackingKey {
                    pool: [1; 32],
                    generation: [2; 16],
                })
            } else {
                None
            },
        });
        spec
    }

    /// AC-T7.4.1: identical OCI layer representation may mean native Windows or
    /// Linux in Shards. Only the admitted execution/platform determines its side.
    #[test]
    fn committed_platform_routes_linux_oci_on_mac_and_both_windows_paths() {
        let linux = routed(ImageOs::Linux, Execution::ShardsShared);
        let windows = routed(ImageOs::Windows, Execution::Native);
        assert_eq!(
            committed_side(&linux, host(ImageOs::Macos, ImageArch::Arm64)).unwrap(),
            Some(Side::Guest)
        );
        assert_eq!(
            committed_side(&linux, host(ImageOs::Windows, ImageArch::Arm64)).unwrap(),
            Some(Side::Guest)
        );
        assert_eq!(
            committed_side(&windows, host(ImageOs::Windows, ImageArch::Arm64)).unwrap(),
            Some(Side::Host)
        );
        assert!(committed_side(&windows, host(ImageOs::Macos, ImageArch::Arm64)).is_err());
        assert!(committed_side(&linux, host(ImageOs::Windows, ImageArch::Amd64)).is_err());
    }

    /// AC-T7.4.1: a caller cannot relabel a guest receipt as native or bypass its
    /// chosen GPU/backing relationship; no backend/probe/boot runs in this control.
    #[test]
    fn committed_route_refuses_forged_execution_and_device_mode() {
        let mut spec = routed(ImageOs::Linux, Execution::ShardsShared);
        spec.route.as_mut().unwrap().key.execution = Execution::Native;
        assert!(committed_side(&spec, host(ImageOs::Windows, ImageArch::Arm64)).is_err());
        spec.route.as_mut().unwrap().key.execution = Execution::ShardsShared;
        spec.devices.push(hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        });
        assert!(committed_side(&spec, host(ImageOs::Windows, ImageArch::Arm64)).is_err());
        spec.devices.clear();
        spec.route = None;
        assert_eq!(
            committed_side(&spec, host(ImageOs::Windows, ImageArch::Arm64)).unwrap(),
            None,
            "missing provenance is not a guessed Windows or Linux image"
        );
    }
}
