package container

import (
	"os"
	"path/filepath"
	"testing"
)

func TestReadEnvFileParsesKeyValuePairs(t *testing.T) {
	tmp := t.TempDir()
	envPath := filepath.Join(tmp, ".env")
	os.WriteFile(envPath, []byte("# comment\nANTHROPIC_API_KEY=sk-test-123\nIRRELEVANT=ignored\n"), 0o644)

	result := readEnvFile(envPath, []string{"ANTHROPIC_API_KEY"})
	if v, ok := result["ANTHROPIC_API_KEY"]; !ok || v != "sk-test-123" {
		t.Errorf("expected ANTHROPIC_API_KEY=sk-test-123, got %q", v)
	}
	if _, ok := result["IRRELEVANT"]; ok {
		t.Error("should not include unrequested keys")
	}
}

func TestReadEnvFileStripsQuotes(t *testing.T) {
	tmp := t.TempDir()
	envPath := filepath.Join(tmp, ".env")
	os.WriteFile(envPath, []byte("KEY1=\"quoted\"\nKEY2='single'\n"), 0o644)

	result := readEnvFile(envPath, []string{"KEY1", "KEY2"})
	if v := result["KEY1"]; v != "quoted" {
		t.Errorf("expected 'quoted', got %q", v)
	}
	if v := result["KEY2"]; v != "single" {
		t.Errorf("expected 'single', got %q", v)
	}
}

func TestReadEnvFileMissingFileReturnsEmpty(t *testing.T) {
	result := readEnvFile("/nonexistent/.env", []string{"KEY"})
	if len(result) != 0 {
		t.Errorf("expected empty result, got %d entries", len(result))
	}
}

func TestReadEnvFileSkipsEmptyValues(t *testing.T) {
	tmp := t.TempDir()
	envPath := filepath.Join(tmp, ".env")
	os.WriteFile(envPath, []byte("EMPTY=\nVALID=yes\n"), 0o644)

	result := readEnvFile(envPath, []string{"EMPTY", "VALID"})
	if _, ok := result["EMPTY"]; ok {
		t.Error("should not include empty values")
	}
	if v := result["VALID"]; v != "yes" {
		t.Errorf("expected 'yes', got %q", v)
	}
}

func TestBuildContainerArgsIncludesExpectedFlags(t *testing.T) {
	mounts := []VolumeMount{
		{
			HostPath:      "/home/mk/project",
			ContainerPath: "/workspace/project",
			ReadOnly:      true,
			Exclude:       []string{"node_modules"},
		},
		{
			HostPath:      "/home/mk/data",
			ContainerPath: "/workspace/group",
			ReadOnly:      false,
		},
	}

	args := BuildContainerArgs(mounts, "test-container", "intercom-agent:latest", "UTC")

	assertContains(t, args, "-i")
	assertContains(t, args, "--rm")
	assertContains(t, args, "--name")
	assertContains(t, args, "test-container")
	assertContains(t, args, "TZ=UTC")
	assertContains(t, args, "/home/mk/project:/workspace/project:ro")
	assertContains(t, args, "/home/mk/data:/workspace/group")
	assertContains(t, args, "type=tmpfs,destination=/workspace/project/node_modules,tmpfs-size=0")

	// Image should be last
	if args[len(args)-1] != "intercom-agent:latest" {
		t.Errorf("expected image as last arg, got %q", args[len(args)-1])
	}
}

func TestBuildPoolContainerArgsOmitsRm(t *testing.T) {
	args := BuildPoolContainerArgs(nil, "pool-test", "intercom-agent:latest", "UTC")

	assertContains(t, args, "-i")
	assertContains(t, args, "--name")
	assertContains(t, args, "pool-test")

	for _, a := range args {
		if a == "--rm" {
			t.Error("--rm should NOT be present for pool containers")
		}
	}
}

func assertContains(t *testing.T, args []string, want string) {
	t.Helper()
	for _, a := range args {
		if a == want {
			return
		}
	}
	t.Errorf("expected args to contain %q, got %v", want, args)
}
