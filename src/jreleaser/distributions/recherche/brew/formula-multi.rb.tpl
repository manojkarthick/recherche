class {{brewFormulaName}} < Formula
  desc "{{projectDescription}}"
  homepage "{{projectLinkHomepage}}"
  version "{{projectVersion}}"
  license "{{projectLicense}}"

  {{brewMultiPlatform}}

  on_macos do
    depends_on arch: :arm64
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink "#{libexec}/bin/recherche" => "recherche"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/recherche --version")
  end
end
