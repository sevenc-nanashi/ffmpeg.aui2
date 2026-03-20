fn main() -> anyhow::Result<()> {
    let build = vergen::BuildBuilder::all_build()?;

    vergen::Emitter::default()
        .add_instructions(&build)?
        .emit()?;
    Ok(())
}
