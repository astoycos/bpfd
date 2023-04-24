/*
Copyright 2022.

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

package bpfdagent

import (
	"context"
	"fmt"

	"k8s.io/apimachinery/pkg/types"

	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/builder"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/log"
	"sigs.k8s.io/controller-runtime/pkg/predicate"
	"sigs.k8s.io/controller-runtime/pkg/source"

	bpfdiov1alpha1 "github.com/redhat-et/bpfd/bpfd-operator/apis/v1alpha1"
	bpfdagentinternal "github.com/redhat-et/bpfd/bpfd-operator/controllers/bpfd-agent/internal"

	gobpfd "github.com/redhat-et/bpfd/clients/gobpfd/v1"
	v1 "k8s.io/api/core/v1"
)

//+kubebuilder:rbac:groups=bpfd.io,resources=xdpprograms,verbs=get;list;watch

// BpfProgramReconciler reconciles a BpfProgram object
type XdpProgramReconciler struct {
	ReconcilerCommon
	currentXdpProgram *bpfdiov1alpha1.XdpProgram
	ourNode           *v1.Node
}

func (r *XdpProgramReconciler) getRecCommon() *ReconcilerCommon {
	return &r.ReconcilerCommon
}

// Must match with bpfd internal types
func xdpProceedOnToInt(proceedOn []bpfdiov1alpha1.XdpProceedOnValue) []int32 {
	var out []int32

	for _, p := range proceedOn {
		switch p {
		case "aborted":
			out = append(out, 0)
		case "drop":
			out = append(out, 1)
		case "pass":
			out = append(out, 2)
		case "tx":
			out = append(out, 3)
		case "redirect":
			out = append(out, 4)
		case "dispatcher_return":
			out = append(out, 31)
		}
	}

	return out
}

// SetupWithManager sets up the controller with the Manager.
// The Bpfd-Agent should reconcile whenever a BpfProgramConfig is updated,
// load the program to the node via bpfd, and then create a bpfProgram object
// to reflect per node state information.
func (r *XdpProgramReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&bpfdiov1alpha1.XdpProgram{}, builder.WithPredicates(predicate.And(predicate.GenerationChangedPredicate{}, predicate.ResourceVersionChangedPredicate{}))).
		Owns(&bpfdiov1alpha1.BpfProgram{}, builder.WithPredicates(predicate.And(predicate.GenerationChangedPredicate{}, predicate.ResourceVersionChangedPredicate{}))).
		// Only trigger reconciliation if node labels change since that could
		// make the BpfProgramConfig no longer select the Node. Additionally only
		// care about node events specific to our node
		Watches(
			&source.Kind{Type: &v1.Node{}},
			&handler.EnqueueRequestForObject{},
			builder.WithPredicates(predicate.And(predicate.LabelChangedPredicate{}, nodePredicate(r.NodeName))),
		).
		Complete(r)
}

func (r *XdpProgramReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	r.Logger = log.FromContext(ctx)

	// Lookup K8s node object for this bpfd-agent This should always succeed
	if err := r.Get(ctx, types.NamespacedName{Namespace: v1.NamespaceAll, Name: r.NodeName}, r.ourNode); err != nil {
		return ctrl.Result{Requeue: false}, fmt.Errorf("failed getting bpfd-agent node %s : %v",
			req.NamespacedName, err)
	}

	XdpPrograms := &bpfdiov1alpha1.XdpProgramList{}

	opts := []client.ListOption{}

	if err := r.List(ctx, XdpPrograms, opts...); err != nil {
		return ctrl.Result{Requeue: false}, fmt.Errorf("failed getting XdpPrograms for full reconcile %s : %v",
			req.NamespacedName, err)
	}

	if len(XdpPrograms.Items) == 0 {
		return ctrl.Result{Requeue: false}, nil
	}

	// Get existing ebpf state from bpfd.
	programMap, err := r.listBpfdPrograms(ctx, bpfdagentinternal.Tc)
	if err != nil {
		r.Logger.Error(err, "failed to list loaded bpfd programs")
		return ctrl.Result{Requeue: true, RequeueAfter: retryDurationAgent}, nil
	}

	// Reconcile every XdpProgram Object
	// note: This doesn't necessarily result in any extra grpc calls to bpfd
	for _, XdpProgram := range XdpPrograms.Items {
		r.Logger.Info("bpfd-agent is reconciling", "bpfProgramConfig", XdpProgram.Name)
		r.currentXdpProgram = &XdpProgram
		retry, err := reconcileProgram(ctx, r, r.currentXdpProgram, &r.currentXdpProgram.Spec.BpfProgramCommon, r.ourNode, programMap)
		if err != nil {
			r.Logger.Error(err, "Reconciling BpfProgramConfig Failed", "BpfProgramConfigName", r.currentXdpProgram.Name, "Retrying", retry)
			return ctrl.Result{Requeue: retry, RequeueAfter: retryDurationAgent}, nil
		}
	}

	return ctrl.Result{Requeue: false}, nil
}

// TODO(astoycos) convert this to not operate on the bpfProgramObject
func (r *XdpProgramReconciler) reconcileBpfdPrograms(ctx context.Context,
	existingBpfPrograms map[string]*gobpfd.ListResponse_ListResult,
	bytecode interface{},
	isNodeSelected bool) (bool, error) {

	XdpProgram := r.currentXdpProgram

	ifaces, err := getInterfaces(&XdpProgram.Spec.InterfaceSelector, r.ourNode)
	if err != nil {
		return false, fmt.Errorf("failed to get interfaces for XdpProgram %s: %v", XdpProgram.Name, err)
	}

	bpfProgramEntries := r.bpfProgram.Spec.Programs
	for _, iface := range ifaces {
		loadRequest := &gobpfd.LoadRequest{}

		Id := fmt.Sprintf("%s-%s", XdpProgram.Name, iface)
		loadRequest.Common = bpfdagentinternal.BuildBpfdCommon(bytecode, XdpProgram.Spec.SectionName, bpfdagentinternal.Tc, Id, XdpProgram.Spec.GlobalData)

		loadRequest.AttachInfo = &gobpfd.LoadRequest_XdpAttachInfo{
			XdpAttachInfo: &gobpfd.XDPAttachInfo{
				Priority:  XdpProgram.Spec.Priority,
				Iface:     iface,
				ProceedOn: xdpProceedOnToInt(XdpProgram.Spec.ProceedOn),
			},
		}

		existingProgram, doesProgramExist := existingBpfPrograms[Id]
		if !doesProgramExist {
			r.Logger.V(1).Info("XdpProgram doesn't exist on node")

			// If BpfProgramConfig is being deleted just remove finalizer so the
			// owner relationship can take care of cleanup
			if !XdpProgram.DeletionTimestamp.IsZero() {
				return r.removeFinalizer(ctx, XdpProgram, XdpProgramControllerFinalizer)
			}

			// Make sure if we're not selected just exit
			if !isNodeSelected {
				r.Logger.V(1).Info("bpfProgramConfig does not select this node")
				// Write NodeNodeSelected status
				return r.updateStatus(ctx, r.bpfProgram, BpfProgCondNotSelected)

			}

			// otherwise load it
			bpfProgramEntry, err := bpfdagentinternal.LoadBpfdProgram(ctx, r.BpfdClient, loadRequest)
			if err != nil {
				r.Logger.Error(err, "Failed to load XdpProgram")
				return r.updateStatus(ctx, r.bpfProgram, BpfProgCondNotLoaded)
			}

			bpfProgramEntries[Id] = bpfProgramEntry
		}

		// BpfProgram exists but either BpfProgramConfig is being deleted or node is no
		// longer selected....unload program
		if !XdpProgram.DeletionTimestamp.IsZero() || !isNodeSelected {
			r.Logger.V(1).Info("bpfProgram exists on Node but is scheduled for deletion or node is no longer selected", "isDeleted", !XdpProgram.DeletionTimestamp.IsZero(),
				"isSelected", isNodeSelected)
			if controllerutil.ContainsFinalizer(r.bpfProgram, XdpProgramControllerFinalizer) {
				if err := bpfdagentinternal.UnloadBpfdProgram(ctx, r.BpfdClient, Id); err != nil {
					r.Logger.Error(err, "Failed to unload XdpProgram")
					return r.updateStatus(ctx, r.bpfProgram, BpfProgCondNotLoaded)
				}

				r.removeFinalizer(ctx, XdpProgram, XdpProgramControllerFinalizer)

				// If K8s hasn't cleaned up here it means we're no longer selected
				// write NodeNodeSelected status ignoring error (object may not exist)
				return r.updateStatus(ctx, r.bpfProgram, BpfProgCondNotSelected)
			}

			return false, nil
		}

		// BpfProgram exists but is not correct state, unload and recreate
		if !bpfdagentinternal.DoesProgExist(existingProgram, loadRequest) {
			if err := bpfdagentinternal.UnloadBpfdProgram(ctx, r.BpfdClient, Id); err != nil {
				r.Logger.Error(err, "Failed to unload XdpProgram")
				return r.updateStatus(ctx, r.bpfProgram, BpfProgCondNotLoaded)
			}

			bpfProgramEntry, err := bpfdagentinternal.LoadBpfdProgram(ctx, r.BpfdClient, loadRequest)
			if err != nil {
				r.Logger.Error(err, "Failed to load XdpProgram")
				return r.updateStatus(ctx, r.bpfProgram, BpfProgCondNotLoaded)
			}

			bpfProgramEntries[Id] = bpfProgramEntry
		} else {
			// Program already exists, but bpfProgram K8s Object might not be up to date
			if _, ok := r.bpfProgram.Spec.Programs[Id]; !ok {
				maps, err := bpfdagentinternal.GetMapsForUUID(Id)
				if err != nil {
					r.Logger.Error(err, "failed to get bpfProgram's Maps")
					return r.updateStatus(ctx, r.bpfProgram, BpfProgCondNotLoaded)
				}

				bpfProgramEntries[Id] = maps
			} else {
				// Program exists and bpfProgram K8s Object is up to date
				r.Logger.V(1).Info("Ignoring Object Change nothing to do in bpfd")
			}
		}
	}

	r.expectedPrograms = bpfProgramEntries

	return false, nil
}