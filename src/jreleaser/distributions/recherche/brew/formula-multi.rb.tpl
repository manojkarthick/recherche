class {{brewFormulaName}} < Formula
  desc "{{projectDescription}}"
  homepage "{{projectLinkHomepage}}"
  version "{{projectVersion}}"
  license "{{projectLicense}}"

  {{brewMultiPlatform}}

  on_macos do
    depends_on arch: :arm64
    depends_on macos: :sequoia
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink "#{libexec}/bin/recherche" => "recherche"
  end

  service do
    run [opt_bin/"recherche", "serve"]
    keep_alive true
    process_type :background
    log_path var/"log/recherche.log"
    error_log_path var/"log/recherche.log"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/recherche --version")
  end
end
