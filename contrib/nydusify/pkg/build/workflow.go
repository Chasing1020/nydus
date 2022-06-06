// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

package build

import (
	"encoding/json"
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"

	"github.com/google/uuid"
	"github.com/pkg/errors"
	"github.com/sirupsen/logrus"
)

type WorkflowOption struct {
	ChunkDict        string
	TargetDir        string
	NydusImagePath   string
	PrefetchPatterns string
	ImageVersion     string
}

type Workflow struct {
	WorkflowOption
	BuilderVersion      string
	bootstrapPath       string
	blobsDir            string
	backendConfig       string
	parentBootstrapPath string
	builder             *Builder
	lastBlobID          string
}

type debugJSON struct {
	Version string
	Blobs   []string
}

// Dump output json file of every layer to $workdir/bootstraps directory
// for debug or perf analysis purpose
func (workflow *Workflow) buildOutputJSONPath() string {
	return workflow.bootstrapPath + "-output.json"
}

// Get latest built blob from blobs directory
func (workflow *Workflow) getLatestBlobPath() (string, error) {
	var data debugJSON
	jsonBytes, err := ioutil.ReadFile(workflow.buildOutputJSONPath())
	if err != nil {
		return "", err
	}
	if err := json.Unmarshal(jsonBytes, &data); err != nil {
		return "", err
	}
	blobIDs := data.Blobs

	// Record builder version of current build environment for easy
	// debugging and troubleshooting afterwards.
	workflow.BuilderVersion = data.Version

	if len(blobIDs) == 0 {
		return "", nil
	}

	latestBlobID := blobIDs[len(blobIDs)-1]
	if latestBlobID != workflow.lastBlobID {
		workflow.lastBlobID = latestBlobID
		blobPath := filepath.Join(workflow.blobsDir, latestBlobID)
		return blobPath, nil
	}

	return "", nil
}

// NewWorkflow prepare bootstrap and blobs path for layered build workflow
func NewWorkflow(option WorkflowOption) (*Workflow, error) {
	blobsDir := filepath.Join(option.TargetDir, "blobs")
	if err := os.RemoveAll(blobsDir); err != nil {
		return nil, errors.Wrap(err, "Remove blob directory")
	}
	if err := os.MkdirAll(blobsDir, 0755); err != nil {
		return nil, errors.Wrap(err, "Create blob directory")
	}

	backendConfig := fmt.Sprintf(`{"dir": "%s"}`, blobsDir)
	builder := NewBuilder(option.NydusImagePath)

	return &Workflow{
		WorkflowOption: option,
		blobsDir:       blobsDir,
		backendConfig:  backendConfig,
		builder:        builder,
	}, nil
}

// Build nydus bootstrap and blob, returned blobPath's basename is sha256 hex string
func (workflow *Workflow) Build(
	layerDir, whiteoutSpec, parentBootstrapPath, bootstrapPath string, alignedChunk bool,
) (string, error) {
	workflow.bootstrapPath = bootstrapPath

	if parentBootstrapPath != "" {
		workflow.parentBootstrapPath = parentBootstrapPath
	}

	blobPath := filepath.Join(workflow.blobsDir, uuid.NewString())

	if err := workflow.builder.Run(BuilderOption{
		ParentBootstrapPath: workflow.parentBootstrapPath,
		BootstrapPath:       workflow.bootstrapPath,
		RootfsPath:          layerDir,
		PrefetchPatterns:    workflow.PrefetchPatterns,
		WhiteoutSpec:        whiteoutSpec,
		OutputJSONPath:      workflow.buildOutputJSONPath(),
		BlobPath:            blobPath,
		AlignedChunk:        alignedChunk,
		ChunkDict:           workflow.ChunkDict,
		ImageVersion:        workflow.ImageVersion,
	}); err != nil {
		return "", errors.Wrapf(err, "build layer %s", layerDir)
	}

	workflow.parentBootstrapPath = workflow.bootstrapPath

	digestedBlobPath, err := workflow.getLatestBlobPath()
	if err != nil {
		return "", errors.Wrap(err, "get latest blob")
	}

	logrus.Debugf("original: %s. digested: %s", blobPath, digestedBlobPath)

	// Ignore the empty blob file generated by this build.
	blobInfo, err := os.Stat(blobPath)
	if err != nil {
		if os.IsNotExist(err) {
			return "", nil
		}
		return "", err
	}
	if blobInfo.Size() == 0 {
		return "", nil
	}

	// Rename the newly generated blob to its sha256 digest.
	// Because the flow will use the basename as the blob object to be pushed to registry.
	// When `digestedBlobPath` is void, this layer's bootsrap can be pushed meanwhile not for blob
	if digestedBlobPath != "" {
		err = os.Rename(blobPath, digestedBlobPath)
		// It's possible that two blobs that are built with the same digest.
		// It's not fatal during image creation since rafs can access exactly
		// what it wants since the two are the same, though registry only have
		// one blob corresponding to two layers.
		if err != nil && err != os.ErrExist {
			return "", err
		} else if err == os.ErrExist {
			logrus.Warnf("Same blob %s are generated", digestedBlobPath)
			return "", nil
		}
	}

	return digestedBlobPath, nil
}
