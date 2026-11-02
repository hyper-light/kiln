use super::*;

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn pins(images: &[&str]) -> Result<BTreeMap<String, String>, String> {
    images.iter().map(|image| pin_image(image, &format!("sha256:{HASH}")).map(|p| ((*image).into(), p))).collect()
}

#[test]
fn w29_r24_dockerfile_globals_stages_scratch_and_platforms_keep_their_meaning() -> Result<(), String> {
    let source = "ARG VERSION=3.20\nFROM --platform=$BUILDPLATFORM alpine:${VERSION} AS build\nRUN echo build\nFROM build AS reused\nFROM scratch\nCOPY --from=reused /out /out\n";
    assert_eq!(images(source).map_err(|d| format!("{d:?}"))?, ["alpine:3.20"]);
    let output = rewrite(source, &pins(&["alpine:3.20"])?).map_err(|d| format!("{d:?}"))?;
    assert_eq!(output, format!("ARG VERSION=3.20\nFROM --platform=$BUILDPLATFORM alpine@sha256:{HASH} AS build\nRUN echo build\nFROM build AS reused\nFROM scratch\nCOPY --from=reused /out /out\n"));
    Ok(())
}

#[test]
fn w29_r24_dockerfile_continuations_and_heredoc_text_are_not_extra_bases() -> Result<(), String> {
    let source = "FROM \\\n# dropped by Docker\n alpine:3 AS build\nRUN <<EOF\nFROM not-an-image:latest\nEOF\nCOPY <<END /file\nFROM also-not-an-image:latest\nEND\n";
    assert_eq!(images(source).map_err(|d| format!("{d:?}"))?, ["alpine:3"]);
    let output = rewrite(source, &pins(&["alpine:3"])?).map_err(|d| format!("{d:?}"))?;
    assert_eq!(output, format!("FROM alpine@sha256:{HASH} AS build\nRUN <<EOF\nFROM not-an-image:latest\nEOF\nCOPY <<END /file\nFROM also-not-an-image:latest\nEND\n"));
    Ok(())
}

#[test]
fn w29_r24_external_copy_mount_and_syntax_images_are_pinned_too() -> Result<(), String> {
    let source = "# syntax=docker/dockerfile:1\nFROM alpine:3\nCOPY --from=registry.test:5000/tool:v1 /out /out\nRUN --mount=type=bind,from=tool:2,target=/src echo ready\n";
    assert_eq!(images(source).map_err(|d| format!("{d:?}"))?, ["alpine:3", "docker/dockerfile:1", "registry.test:5000/tool:v1", "tool:2"]);
    let output = rewrite(source, &pins(&["alpine:3", "docker/dockerfile:1", "registry.test:5000/tool:v1", "tool:2"])?).map_err(|d| format!("{d:?}"))?;
    assert!(output.starts_with(&format!("# syntax=docker/dockerfile@sha256:{HASH}\n")));
    assert!(output.contains(&format!("COPY --from=registry.test:5000/tool@sha256:{HASH} /out /out\n")));
    assert!(output.contains(&format!("RUN --mount=type=bind,from=tool@sha256:{HASH},target=/src echo ready\n")));
    Ok(())
}

#[test]
fn w29_r24_unresolved_dockerfile_images_and_missing_facts_never_return_partial_text() -> Result<(), String> {
    for source in ["ARG VERSION\nFROM alpine:$VERSION\n", "FROM ${UNKNOWN}\n", "FROM alpine:3\nCOPY --from=$IMAGE /x /x\n", "FROM", "FROM alpine:3\\\n", "FROM alpine:3\nRUN <<EOF\nnever closed\n", "# unknown=value\n# escape=`\nFROM alpine:3\n"] {
        assert!(images(source).is_err(), "{source}");
    }
    assert!(rewrite("FROM alpine:3\nFROM tool:2\n", &pins(&["alpine:3"])?).err().is_some_and(|d| d.iter().any(|d| d.code == "KCI4050")));
    let pinned = format!("FROM alpine@sha256:{HASH}\n");
    assert_eq!(images(&pinned).map_err(|d| format!("{d:?}"))?, [format!("alpine@sha256:{HASH}")]);
    Ok(())
}
