package routing

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// writeFakeIC puts a stub `ic` first on PATH that records its arguments and
// prints the given JSON, shaped like the real `ic route model --json` output.
func writeFakeIC(t *testing.T, output string) (argsFile string) {
	t.Helper()
	dir := t.TempDir()
	argsFile = filepath.Join(dir, "args")
	script := "#!/bin/sh\nprintf '%s\\n' \"$@\" > " + argsFile + "\ncat <<'EOF'\n" + output + "\nEOF\n"
	if err := os.WriteFile(filepath.Join(dir, "ic"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", dir+string(os.PathListSeparator)+os.Getenv("PATH"))
	return argsFile
}

func TestIntercoreRouterInfersRuntimeFromModel(t *testing.T) {
	argsFile := writeFakeIC(t, `{"agent":"","category":"","model":"gpt-6-astra","phase":"scheduled"}`)

	model, runtime, err := NewIntercoreRouter().SelectModel(context.Background(), "scheduled")
	if err != nil {
		t.Fatalf("SelectModel: %v", err)
	}
	if model != "gpt-6-astra" {
		t.Errorf("model = %q, want gpt-6-astra", model)
	}
	if runtime != "codex" {
		t.Errorf("runtime = %q, want codex", runtime)
	}

	raw, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatal(err)
	}
	args := strings.Fields(string(raw))
	want := []string{"route", "model", "--phase=scheduled", "--json"}
	if strings.Join(args, " ") != strings.Join(want, " ") {
		t.Errorf("ic args = %v, want %v", args, want)
	}
}

func TestIntercoreRouterEmptyModelIsError(t *testing.T) {
	writeFakeIC(t, `{"agent":"","category":"","model":"","phase":"default"}`)

	if _, _, err := NewIntercoreRouter().SelectModel(context.Background(), "default"); err == nil {
		t.Fatal("expected error for empty model")
	}

	fb := &FallbackRouter{Primary: NewIntercoreRouter(), Fallback: &ConfigRouter{Model: "m", Runtime: "claude"}}
	model, runtime, err := fb.SelectModel(context.Background(), "default")
	if err != nil || model != "m" || runtime != "claude" {
		t.Errorf("fallback = (%q, %q, %v), want (m, claude, nil)", model, runtime, err)
	}
}

// TestIntercoreRouterRealIC runs the installed ic binary. It skips when ic is
// not on PATH.
func TestIntercoreRouterRealIC(t *testing.T) {
	if _, err := exec.LookPath("ic"); err != nil {
		t.Skip("ic not on PATH")
	}
	t.Chdir(t.TempDir())

	model, runtime, err := NewIntercoreRouter().SelectModel(context.Background(), "executing")
	if err != nil {
		t.Fatalf("SelectModel against real ic: %v", err)
	}
	if model == "" {
		t.Fatal("real ic returned an empty model")
	}
	if runtime == "" || runtime != RuntimeForModel(model) {
		t.Errorf("runtime = %q, want %q (inferred from model %q)", runtime, RuntimeForModel(model), model)
	}
}
