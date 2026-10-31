# The formula lives in the repo and ships through the
# vhaudiquet/homebrew-ecrou tap; it needs the same tap libkrun's
# krunkit comes from, because the engine is dlopen'd from those kegs
# at runtime.
class Ecrou < Formula
  desc "a small microVM runtime based on libkrun"
  homepage "https://github.com/vhaudiquet/ecrou"
  # Placeholder pin: the release workflow rewrites url and sha256 on publish.
  url "https://github.com/vhaudiquet/ecrou.git", tag: "v0.1.0"
  license "MIT"

  # libkrun's Hypervisor.framework backend is arm64-only upstream.
  depends_on arch: :arm64
  depends_on "rust" => :build
  depends_on "libkrun/homebrew-krun/libkrun"
  depends_on "e2fsprogs"
  # A libarchive-capable mke2fs populates ext4 images from tarballs in
  # one pass; without it the build falls back to a per-file tree walk.
  depends_on "libarchive"

  def install
    system "cargo", "build", "--release"
    bin.install "target/release/ecrou"
    # Hypervisor.framework grants VM creation only to binaries signed
    # with the hypervisor entitlement; on current macOS the signature
    # is a hard requirement, the same one krunkit's formula carries.
    system "codesign", "--force", "--sign", "-",
      "--entitlements", "packaging/homebrew/ecrou.entitlements", bin/"ecrou"
  end

  def caveats
    <<~EOS
      The binary is ad-hoc signed with the com.apple.security.hypervisor
      entitlement. Rebuilding from source (cargo) requires re-signing:
        codesign --force --sign - --entitlements packaging/homebrew/ecrou.entitlements target/release/ecrou
      `ecrou probe` reports this when the entitlement is missing.
    EOS
  end

  test do
    system bin/"ecrou", "probe"
    output = shell_output("#{bin}/ecrou run --quiet alpine -- uname -r").strip
    assert_match(/\d+\.\d+/, output)
  end
end
