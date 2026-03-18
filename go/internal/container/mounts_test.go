package container

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func setupProjectDirs(t *testing.T) (projectRoot, groupsDir, dataDir string) {
	t.Helper()
	tmp := t.TempDir()
	projectRoot = filepath.Join(tmp, "project")
	groupsDir = filepath.Join(tmp, "groups")
	dataDir = filepath.Join(tmp, "data")
	os.MkdirAll(projectRoot, 0o755)
	os.MkdirAll(groupsDir, 0o755)
	os.MkdirAll(dataDir, 0o755)
	return
}

func TestMainGroupGetsProjectRootAndGroupDir(t *testing.T) {
	projectRoot, groupsDir, dataDir := setupProjectDirs(t)

	group := &GroupInfo{Folder: "main", Name: "Main Group"}
	mounts := BuildVolumeMounts(group, true, RuntimeClaude,
		projectRoot, groupsDir, dataDir, nil)

	var hasProjectRoot, hasGroupDir bool
	for _, m := range mounts {
		if m.ContainerPath == "/workspace/project" {
			hasProjectRoot = true
			if !m.ReadOnly {
				t.Error("project root should be read-only")
			}
		}
		if m.ContainerPath == "/workspace/group" {
			hasGroupDir = true
			if m.ReadOnly {
				t.Error("group dir should be read-write")
			}
		}
	}
	if !hasProjectRoot {
		t.Error("main group should have project root mount")
	}
	if !hasGroupDir {
		t.Error("main group should have group dir mount")
	}
}

func TestNonMainGroupGetsGlobalMemory(t *testing.T) {
	projectRoot, groupsDir, dataDir := setupProjectDirs(t)

	// Create global directory
	os.MkdirAll(filepath.Join(groupsDir, "global"), 0o755)

	group := &GroupInfo{Folder: "team-eng", Name: "Engineering"}
	mounts := BuildVolumeMounts(group, false, RuntimeClaude,
		projectRoot, groupsDir, dataDir, nil)

	var hasGlobal, hasProject bool
	for _, m := range mounts {
		if m.ContainerPath == "/workspace/global" {
			hasGlobal = true
			if !m.ReadOnly {
				t.Error("global should be read-only")
			}
		}
		if m.ContainerPath == "/workspace/project" {
			hasProject = true
		}
	}
	if !hasGlobal {
		t.Error("non-main should have global mount")
	}
	if hasProject {
		t.Error("non-main should NOT have project root mount")
	}
}

func TestClaudeRuntimeCreatesSessionsDir(t *testing.T) {
	projectRoot, groupsDir, dataDir := setupProjectDirs(t)

	group := &GroupInfo{Folder: "main", Name: "Main"}
	mounts := BuildVolumeMounts(group, true, RuntimeClaude,
		projectRoot, groupsDir, dataDir, nil)

	var hasClaude bool
	for _, m := range mounts {
		if m.ContainerPath == "/home/node/.claude" {
			hasClaude = true
		}
	}
	if !hasClaude {
		t.Error("claude runtime should create .claude sessions mount")
	}

	// Settings file should have been created
	settingsPath := filepath.Join(dataDir, "sessions", "main", ".claude", "settings.json")
	if _, err := os.Stat(settingsPath); os.IsNotExist(err) {
		t.Error("settings.json should have been created")
	}
}

func TestNonClaudeRuntimeSkipsSessionsDir(t *testing.T) {
	projectRoot, groupsDir, dataDir := setupProjectDirs(t)

	group := &GroupInfo{Folder: "main", Name: "Main"}
	mounts := BuildVolumeMounts(group, true, RuntimeGemini,
		projectRoot, groupsDir, dataDir, nil)

	for _, m := range mounts {
		if m.ContainerPath == "/home/node/.claude" {
			t.Error("gemini runtime should NOT create .claude sessions mount")
		}
	}
}

func TestIPCDirectoriesCreated(t *testing.T) {
	projectRoot, groupsDir, dataDir := setupProjectDirs(t)

	group := &GroupInfo{Folder: "main", Name: "Main"}
	BuildVolumeMounts(group, true, RuntimeClaude,
		projectRoot, groupsDir, dataDir, nil)

	ipcBase := filepath.Join(dataDir, "ipc", "main")
	for _, sub := range []string{"messages", "tasks", "input", "queries", "responses"} {
		if _, err := os.Stat(filepath.Join(ipcBase, sub)); os.IsNotExist(err) {
			t.Errorf("IPC subdirectory %q should have been created", sub)
		}
	}
}

func TestContainerNameSanitizesFolder(t *testing.T) {
	name := ContainerName("team.eng/special")
	if !strings.HasPrefix(name, "intercom-team-eng-special-") {
		t.Errorf("unexpected name prefix: %q", name)
	}
	if strings.Contains(name[:len(name)-13], ".") { // strip timestamp suffix
		t.Error("container name should not contain '.'")
	}
	if strings.Contains(name[:len(name)-13], "/") {
		t.Error("container name should not contain '/'")
	}
}
