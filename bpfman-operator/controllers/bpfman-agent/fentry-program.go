/*
Copyright 2024.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

package bpfmanagent

import (
	"context"
	"fmt"
	"strings"

	bpfmaniov1alpha1 "github.com/bpfman/bpfman/bpfman-operator/apis/v1alpha1"
	bpfmanagentinternal "github.com/bpfman/bpfman/bpfman-operator/controllers/bpfman-agent/internal"
	"github.com/bpfman/bpfman/bpfman-operator/internal"
	gobpfman "github.com/bpfman/bpfman/clients/gobpfman/v1"

	v1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/builder"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/log"
	"sigs.k8s.io/controller-runtime/pkg/predicate"
	"sigs.k8s.io/controller-runtime/pkg/source"
)

//+kubebuilder:rbac:groups=bpfman.io,resources=fentryprograms,verbs=get;list;watch

// BpfProgramReconciler reconciles a BpfProgram object
type FentryProgramReconciler struct {
	ReconcilerCommon
	currentFentryProgram *bpfmaniov1alpha1.FentryProgram
	ourNode              *v1.Node
}

func (r *FentryProgramReconciler) getRecCommon() *ReconcilerCommon {
	return &r.ReconcilerCommon
}

func (r *FentryProgramReconciler) getFinalizer() string {
	return internal.FentryProgramControllerFinalizer
}

func (r *FentryProgramReconciler) getRecType() string {
	return internal.Tracing.String()
}

// SetupWithManager sets up the controller with the Manager.
// The Bpfman-Agent should reconcile whenever a FentryProgram is updated,
// load the program to the node via bpfman, and then create a bpfProgram object
// to reflect per node state information.
func (r *FentryProgramReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&bpfmaniov1alpha1.FentryProgram{}, builder.WithPredicates(predicate.And(predicate.GenerationChangedPredicate{}, predicate.ResourceVersionChangedPredicate{}))).
		Owns(&bpfmaniov1alpha1.BpfProgram{},
			builder.WithPredicates(predicate.And(
				internal.BpfProgramTypePredicate(internal.Tracing.String()),
				internal.BpfProgramNodePredicate(r.NodeName)),
			),
		).
		// Only trigger reconciliation if node labels change since that could
		// make the FentryProgram no longer select the Node. Additionally only
		// care about node events specific to our node
		Watches(
			&source.Kind{Type: &v1.Node{}},
			&handler.EnqueueRequestForObject{},
			builder.WithPredicates(predicate.And(predicate.LabelChangedPredicate{}, nodePredicate(r.NodeName))),
		).
		Complete(r)
}

func (r *FentryProgramReconciler) expectedBpfPrograms(ctx context.Context) (*bpfmaniov1alpha1.BpfProgramList, error) {
	progs := &bpfmaniov1alpha1.BpfProgramList{}

	// sanitize fentry name to work in a bpfProgram name
	sanatizedFentry := strings.Replace(strings.Replace(r.currentFentryProgram.Spec.FunctionName, "/", "-", -1), "_", "-", -1)
	bpfProgramName := fmt.Sprintf("%s-%s-%s", r.currentFentryProgram.Name, r.NodeName, sanatizedFentry)

	annotations := map[string]string{internal.FentryProgramFunction: r.currentFentryProgram.Spec.FunctionName}

	prog, err := r.createBpfProgram(ctx, bpfProgramName, r.getFinalizer(), r.currentFentryProgram, r.getRecType(), annotations)
	if err != nil {
		return nil, fmt.Errorf("failed to create BpfProgram %s: %v", bpfProgramName, err)
	}

	progs.Items = append(progs.Items, *prog)

	return progs, nil
}

func (r *FentryProgramReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	// Initialize node and current program
	r.currentFentryProgram = &bpfmaniov1alpha1.FentryProgram{}
	r.ourNode = &v1.Node{}
	r.Logger = ctrl.Log.WithName("fentry")

	ctxLogger := log.FromContext(ctx)
	ctxLogger.Info("Reconcile Fentry: Enter", "ReconcileKey", req)

	// Lookup K8s node object for this bpfman-agent This should always succeed
	if err := r.Get(ctx, types.NamespacedName{Namespace: v1.NamespaceAll, Name: r.NodeName}, r.ourNode); err != nil {
		return ctrl.Result{Requeue: false}, fmt.Errorf("failed getting bpfman-agent node %s : %v",
			req.NamespacedName, err)
	}

	fentryPrograms := &bpfmaniov1alpha1.FentryProgramList{}

	opts := []client.ListOption{}

	if err := r.List(ctx, fentryPrograms, opts...); err != nil {
		return ctrl.Result{Requeue: false}, fmt.Errorf("failed getting FentryPrograms for full reconcile %s : %v",
			req.NamespacedName, err)
	}

	if len(fentryPrograms.Items) == 0 {
		r.Logger.Info("FentryProgramController found no Fentry Programs")
		return ctrl.Result{Requeue: false}, nil
	}

	// Get existing ebpf state from bpfman.
	programMap, err := bpfmanagentinternal.ListBpfmanPrograms(ctx, r.BpfmanClient, internal.Tracing)
	if err != nil {
		r.Logger.Error(err, "failed to list loaded bpfman programs")
		return ctrl.Result{Requeue: true, RequeueAfter: retryDurationAgent}, nil
	}

	// Reconcile each FentryProgram. Don't return error here because it will trigger an infinite reconcile loop, instead
	// report the error to user and retry if specified. For some errors the controller may not decide to retry.
	// Note: This only results in grpc calls to bpfman if we need to change something
	requeue := false // initialize requeue to false
	for _, fentryProgram := range fentryPrograms.Items {
		r.Logger.Info("FentryProgramController is reconciling", "currentFentryProgram", fentryProgram.Name)
		r.currentFentryProgram = &fentryProgram
		result, err := reconcileProgram(ctx, r, r.currentFentryProgram, &r.currentFentryProgram.Spec.BpfProgramCommon, r.ourNode, programMap)
		if err != nil {
			r.Logger.Error(err, "Reconciling FentryProgram Failed", "FentryProgramName", r.currentFentryProgram.Name, "ReconcileResult", result.String())
		}

		switch result {
		case internal.Unchanged:
			// continue with next program
		case internal.Updated:
			// return
			return ctrl.Result{Requeue: false}, nil
		case internal.Requeue:
			// remember to do a requeue when we're done and continue with next program
			requeue = true
		}
	}

	if requeue {
		// A requeue has been requested
		return ctrl.Result{RequeueAfter: retryDurationAgent}, nil
	} else {
		// We've made it through all the programs in the list without anything being
		// updated and a reque has not been requested.
		return ctrl.Result{Requeue: false}, nil
	}
}

func (r *FentryProgramReconciler) buildFentryLoadRequest(
	bytecode *gobpfman.BytecodeLocation,
	uuid string,
	bpfProgram *bpfmaniov1alpha1.BpfProgram,
	mapOwnerId *uint32) *gobpfman.LoadRequest {

	return &gobpfman.LoadRequest{
		Bytecode:    bytecode,
		Name:        r.currentFentryProgram.Spec.BpfFunctionName,
		ProgramType: uint32(internal.Tracing),
		Attach: &gobpfman.AttachInfo{
			Info: &gobpfman.AttachInfo_FentryAttachInfo{
				FentryAttachInfo: &gobpfman.FentryAttachInfo{
					FnName: bpfProgram.Annotations[internal.FentryProgramFunction],
				},
			},
		},
		Metadata:   map[string]string{internal.UuidMetadataKey: uuid, internal.ProgramNameKey: r.currentFentryProgram.Name},
		GlobalData: r.currentFentryProgram.Spec.GlobalData,
		MapOwnerId: mapOwnerId,
	}
}

// reconcileBpfmanPrograms ONLY reconciles the bpfman state for a single BpfProgram.
// It does not interact with the k8s API in any way.
func (r *FentryProgramReconciler) reconcileBpfmanProgram(ctx context.Context,
	existingBpfPrograms map[string]*gobpfman.ListResponse_ListResult,
	bytecodeSelector *bpfmaniov1alpha1.BytecodeSelector,
	bpfProgram *bpfmaniov1alpha1.BpfProgram,
	isNodeSelected bool,
	isBeingDeleted bool,
	mapOwnerStatus *MapOwnerParamStatus) (bpfmaniov1alpha1.BpfProgramConditionType, error) {

	r.Logger.V(1).Info("Existing bpfProgram", "UUID", bpfProgram.UID, "Name", bpfProgram.Name, "CurrentFentryProgram", r.currentFentryProgram.Name)

	uuid := bpfProgram.UID

	getLoadRequest := func() (*gobpfman.LoadRequest, bpfmaniov1alpha1.BpfProgramConditionType, error) {
		bytecode, err := bpfmanagentinternal.GetBytecode(r.Client, bytecodeSelector)
		if err != nil {
			return nil, bpfmaniov1alpha1.BpfProgCondBytecodeSelectorError, fmt.Errorf("failed to process bytecode selector: %v", err)
		}
		loadRequest := r.buildFentryLoadRequest(bytecode, string(uuid), bpfProgram, mapOwnerStatus.mapOwnerId)
		return loadRequest, bpfmaniov1alpha1.BpfProgCondNone, nil
	}

	existingProgram, doesProgramExist := existingBpfPrograms[string(uuid)]
	if !doesProgramExist {
		r.Logger.V(1).Info("FentryProgram doesn't exist on node")

		// If FentryProgram is being deleted just exit
		if isBeingDeleted {
			return bpfmaniov1alpha1.BpfProgCondUnloaded, nil
		}

		// Make sure if we're not selected just exit
		if !isNodeSelected {
			return bpfmaniov1alpha1.BpfProgCondNotSelected, nil
		}

		// Make sure if the Map Owner is set but not found then just exit
		if mapOwnerStatus.isSet && !mapOwnerStatus.isFound {
			return bpfmaniov1alpha1.BpfProgCondMapOwnerNotFound, nil
		}

		// Make sure if the Map Owner is set but not loaded then just exit
		if mapOwnerStatus.isSet && !mapOwnerStatus.isLoaded {
			return bpfmaniov1alpha1.BpfProgCondMapOwnerNotLoaded, nil
		}

		// otherwise load it
		loadRequest, condition, err := getLoadRequest()
		if err != nil {
			return condition, err
		}

		r.progId, err = bpfmanagentinternal.LoadBpfmanProgram(ctx, r.BpfmanClient, loadRequest)
		if err != nil {
			r.Logger.Error(err, "Failed to load FentryProgram")
			return bpfmaniov1alpha1.BpfProgCondNotLoaded, nil
		}

		r.Logger.Info("bpfman called to load FentryProgram on Node", "Name", bpfProgram.Name, "UUID", uuid)
		return bpfmaniov1alpha1.BpfProgCondLoaded, nil
	}

	// prog ID should already have been set if program exists
	id, err := bpfmanagentinternal.GetID(bpfProgram)
	if err != nil {
		r.Logger.Error(err, "Failed to get program ID")
		return bpfmaniov1alpha1.BpfProgCondNotLoaded, nil
	}

	// BpfProgram exists but either FentryProgram is being deleted, node is no
	// longer selected, or map is not available....unload program
	if isBeingDeleted || !isNodeSelected ||
		(mapOwnerStatus.isSet && (!mapOwnerStatus.isFound || !mapOwnerStatus.isLoaded)) {
		r.Logger.V(1).Info("FentryProgram exists on Node but is scheduled for deletion, not selected, or map not available",
			"isDeleted", isBeingDeleted, "isSelected", isNodeSelected, "mapIsSet", mapOwnerStatus.isSet,
			"mapIsFound", mapOwnerStatus.isFound, "mapIsLoaded", mapOwnerStatus.isLoaded, "id", id)

		if err := bpfmanagentinternal.UnloadBpfmanProgram(ctx, r.BpfmanClient, *id); err != nil {
			r.Logger.Error(err, "Failed to unload FentryProgram")
			return bpfmaniov1alpha1.BpfProgCondNotUnloaded, nil
		}

		r.Logger.Info("bpfman called to unload FentryProgram on Node", "Name", bpfProgram.Name, "UUID", uuid)

		if isBeingDeleted {
			return bpfmaniov1alpha1.BpfProgCondUnloaded, nil
		}

		if !isNodeSelected {
			return bpfmaniov1alpha1.BpfProgCondNotSelected, nil
		}

		if mapOwnerStatus.isSet && !mapOwnerStatus.isFound {
			return bpfmaniov1alpha1.BpfProgCondMapOwnerNotFound, nil
		}

		if mapOwnerStatus.isSet && !mapOwnerStatus.isLoaded {
			return bpfmaniov1alpha1.BpfProgCondMapOwnerNotLoaded, nil
		}
	}

	// BpfProgram exists but is not correct state, unload and recreate
	loadRequest, condition, err := getLoadRequest()
	if err != nil {
		return condition, err
	}

	r.Logger.V(1).WithValues("expectedProgram", loadRequest).WithValues("existingProgram", existingProgram).Info("StateMatch")

	isSame, reasons := bpfmanagentinternal.DoesProgExist(existingProgram, loadRequest)
	if !isSame {
		r.Logger.V(1).Info("FentryProgram is in wrong state, unloading and reloading", "Reason", reasons)
		if err := bpfmanagentinternal.UnloadBpfmanProgram(ctx, r.BpfmanClient, *id); err != nil {
			r.Logger.Error(err, "Failed to unload FentryProgram")
			return bpfmaniov1alpha1.BpfProgCondNotUnloaded, nil
		}

		r.progId, err = bpfmanagentinternal.LoadBpfmanProgram(ctx, r.BpfmanClient, loadRequest)
		if err != nil {
			r.Logger.Error(err, "Failed to load FentryProgram")
			return bpfmaniov1alpha1.BpfProgCondNotLoaded, err
		}

		r.Logger.Info("bpfman called to reload FentryProgram on Node", "Name", bpfProgram.Name, "UUID", uuid)
	} else {
		// Program exists and bpfProgram K8s Object is up to date
		r.Logger.V(1).Info("Ignoring Object Change nothing to do in bpfman")
		r.progId = id
	}

	return bpfmaniov1alpha1.BpfProgCondLoaded, nil
}
