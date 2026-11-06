//! bp-g2 step 3: the TypeScript codec is generated from kiln-bp's types and checked in
//! (`ui/src/api/bp/gen.ts`). This fails, naming the file and its first difference, when a type
//! changes without it; `KILN_WRITE_BP=1 cargo test -p kiln-bp --test ts` writes it.

#[test]
fn bp_g2_s3_the_typescript_codec_is_current() -> Result<(), String> {
    let want = kiln_bp::ts::render();
    let path = kiln_bp::ts::path();
    if std::env::var("KILN_WRITE_BP").is_ok() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, &want).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(());
    }
    let have = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e} (run KILN_WRITE_BP=1 cargo test -p kiln-bp --test ts)", path.display()))?;
    if have != want {
        let line = have.lines().zip(want.lines()).position(|(a, b)| a != b).unwrap_or_else(|| have.lines().count().min(want.lines().count()));
        return Err(format!("{} differs from kiln-bp's types at line {}: run KILN_WRITE_BP=1 cargo test -p kiln-bp --test ts", path.display(), line + 1));
    }
    Ok(())
}
