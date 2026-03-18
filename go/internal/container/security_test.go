package container

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func testAllowlist(tmp string) *MountAllowlist {
	return &MountAllowlist{
		AllowedRoots: []AllowedRoot{{
			Path:           tmp,
			AllowReadWrite: true,
			Description:    "test root",
		}},
		BlockedPatterns: append([]string(nil), defaultBlockedPatterns...),
		NonMainReadOnly: true,
	}
}

func TestAllowsPathUnderAllowedRoot(t *testing.T) {
	tmp := t.TempDir()
	sub := filepath.Join(tmp, "project")
	os.MkdirAll(sub, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath:      sub,
		ContainerPath: "project",
		ReadOnly:      true,
	}

	result := ValidateMount(mount, true, allowlist)
	if !result.Allowed {
		t.Errorf("expected allowed, reason: %s", result.Reason)
	}
	if result.ResolvedContainerPath != "project" {
		t.Errorf("expected resolved path 'project', got %q", result.ResolvedContainerPath)
	}
}

func TestBlocksPathNotUnderAllowedRoot(t *testing.T) {
	tmp := t.TempDir()
	other := t.TempDir()
	sub := filepath.Join(other, "secret")
	os.MkdirAll(sub, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath: sub,
		ReadOnly: true,
	}

	result := ValidateMount(mount, true, allowlist)
	if result.Allowed {
		t.Error("expected blocked for path not under allowed root")
	}
	if !strings.Contains(result.Reason, "not under any allowed root") {
		t.Errorf("unexpected reason: %s", result.Reason)
	}
}

func TestBlocksSSHDirectory(t *testing.T) {
	tmp := t.TempDir()
	sshDir := filepath.Join(tmp, ".ssh")
	os.MkdirAll(sshDir, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath: sshDir,
		ReadOnly: true,
	}

	result := ValidateMount(mount, true, allowlist)
	if result.Allowed {
		t.Error("expected .ssh to be blocked")
	}
	if !strings.Contains(result.Reason, ".ssh") {
		t.Errorf("expected reason to mention .ssh, got: %s", result.Reason)
	}
}

func TestBlocksPathTraversalInContainerPath(t *testing.T) {
	tmp := t.TempDir()
	sub := filepath.Join(tmp, "ok")
	os.MkdirAll(sub, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath:      sub,
		ContainerPath: "../../etc/passwd",
		ReadOnly:      true,
	}

	result := ValidateMount(mount, true, allowlist)
	if result.Allowed {
		t.Error("expected path traversal to be blocked")
	}
	if !strings.Contains(result.Reason, "..") {
		t.Errorf("expected reason to mention '..', got: %s", result.Reason)
	}
}

func TestNonMainForcedReadOnly(t *testing.T) {
	tmp := t.TempDir()
	sub := filepath.Join(tmp, "data")
	os.MkdirAll(sub, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath:      sub,
		ContainerPath: "data",
		ReadOnly:      false, // requests read-write
	}

	result := ValidateMount(mount, false, allowlist)
	if !result.Allowed {
		t.Errorf("expected allowed, reason: %s", result.Reason)
	}
	if result.EffectiveReadOnly == nil || !*result.EffectiveReadOnly {
		t.Error("expected forced read-only for non-main group")
	}
}

func TestMainGetsReadWrite(t *testing.T) {
	tmp := t.TempDir()
	sub := filepath.Join(tmp, "data")
	os.MkdirAll(sub, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath:      sub,
		ContainerPath: "data",
		ReadOnly:      false,
	}

	result := ValidateMount(mount, true, allowlist)
	if !result.Allowed {
		t.Errorf("expected allowed, reason: %s", result.Reason)
	}
	if result.EffectiveReadOnly == nil || *result.EffectiveReadOnly {
		t.Error("expected read-write for main group")
	}
}

func TestNonexistentPathRejected(t *testing.T) {
	tmp := t.TempDir()
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath: "/nonexistent/path/to/nowhere",
		ReadOnly: true,
	}

	result := ValidateMount(mount, true, allowlist)
	if result.Allowed {
		t.Error("expected nonexistent path to be rejected")
	}
	if !strings.Contains(result.Reason, "does not exist") {
		t.Errorf("expected reason to mention 'does not exist', got: %s", result.Reason)
	}
}

func TestValidateAdditionalMountsFiltersInvalid(t *testing.T) {
	tmp := t.TempDir()
	good := filepath.Join(tmp, "good")
	os.MkdirAll(good, 0o755)
	allowlist := testAllowlist(tmp)

	mounts := []AdditionalMount{
		{HostPath: good, ContainerPath: "good", ReadOnly: true},
		{HostPath: "/nonexistent", ReadOnly: true},
	}

	validated := ValidateAdditionalMounts(mounts, "test-group", true, allowlist)
	if len(validated) != 1 {
		t.Fatalf("expected 1 validated mount, got %d", len(validated))
	}
	if validated[0].ContainerPath != "/workspace/extra/good" {
		t.Errorf("expected '/workspace/extra/good', got %q", validated[0].ContainerPath)
	}
}

func TestContainerPathDefaultsToBasename(t *testing.T) {
	tmp := t.TempDir()
	sub := filepath.Join(tmp, "my-project")
	os.MkdirAll(sub, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath: sub,
		ReadOnly: true,
	}

	result := ValidateMount(mount, true, allowlist)
	if !result.Allowed {
		t.Errorf("expected allowed, reason: %s", result.Reason)
	}
	if result.ResolvedContainerPath != "my-project" {
		t.Errorf("expected 'my-project', got %q", result.ResolvedContainerPath)
	}
}

func TestAbsoluteContainerPathRejected(t *testing.T) {
	tmp := t.TempDir()
	sub := filepath.Join(tmp, "ok")
	os.MkdirAll(sub, 0o755)
	allowlist := testAllowlist(tmp)

	mount := AdditionalMount{
		HostPath:      sub,
		ContainerPath: "/etc/bad",
		ReadOnly:      true,
	}

	result := ValidateMount(mount, true, allowlist)
	if result.Allowed {
		t.Error("expected absolute container path to be rejected")
	}
}
