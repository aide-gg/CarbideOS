// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    // The software renderer has no shader pipeline, so the embedded scale
    // factor is the only way to get crisp output on high-DPI panels.
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/app.slint", config).expect("slint build failed");
}
