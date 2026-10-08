fn main() {
    // The search bar's edit box needs Common Controls 6 (its placeholder
    // text): a manifest saying so, embedded in fsearchd.exe.
    println!("cargo:rustc-link-arg-bin=fsearchd=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=fsearchd=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
