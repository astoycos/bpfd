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
	"reflect"
	"time"

	v1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/event"
	"sigs.k8s.io/controller-runtime/pkg/predicate"

	bpfdiov1alpha1 "github.com/bpfd-dev/bpfd/bpfd-operator/apis/v1alpha1"
	bpfdagentinternal "github.com/bpfd-dev/bpfd/bpfd-operator/controllers/bpfd-agent/internal"
	"github.com/bpfd-dev/bpfd/bpfd-operator/internal"
	gobpfd "github.com/bpfd-dev/bpfd/clients/gobpfd/v1"
	"github.com/go-logr/logr"
	"google.golang.org/grpc"
)

//+kubebuilder:rbac:groups=bpfd.dev,resources=bpfprograms,verbs=get;list;watch;create;update;patch;delete
//+kubebuilder:rbac:groups=bpfd.dev,resources=bpfprograms/status,verbs=get;update;patch
//+kubebuilder:rbac:groups=bpfd.dev,resources=bpfprograms/finalizers,verbs=update
//+kubebuilder:rbac:groups=bpfd.dev,resources=tcprograms/finalizers,verbs=update
//+kubebuilder:rbac:groups=bpfd.dev,resources=xdpprograms/finalizers,verbs=update
//+kubebuilder:rbac:groups=bpfd.dev,resources=tracepointprograms/finalizers,verbs=update
//+kubebuilder:rbac:groups=bpfd.dev,resources=kprobeprograms/finalizers,verbs=update
//+kubebuilder:rbac:groups=core,resources=nodes,verbs=get;list;watch
//+kubebuilder:rbac:groups=core,resources=secrets,verbs=get

const (
	retryDurationAgent = 5 * time.Second
)

// ReconcilerCommon provides a skeleton for all *Program Reconcilers.
type ReconcilerCommon struct {
	client.Client
	Scheme       *runtime.Scheme
	GrpcConn     *grpc.ClientConn
	BpfdClient   gobpfd.LoaderClient
	Logger       logr.Logger
	NodeName     string
	expectedMaps map[string]string
}

// bpfdReconciler defines a generic bpfProgram K8s object reconciler which can
// program bpfd from user intent in K8s CRDs.
type bpfdReconciler interface {
	getRecCommon() *ReconcilerCommon
	reconcileBpfdProgram(context.Context,
		map[string]*gobpfd.ListResponse_ListResult,
		interface{},
		*bpfdiov1alpha1.BpfProgram,
		bool,
		bool,
		*MapOwnerParamStatus) (bpfdiov1alpha1.BpfProgramConditionType, error)
	getFinalizer() string
	getRecType() string
	expectedBpfPrograms(ctx context.Context) (*bpfdiov1alpha1.BpfProgramList, error)
}

// Only return node updates for our node (all events)
func nodePredicate(nodeName string) predicate.Funcs {
	return predicate.Funcs{
		GenericFunc: func(e event.GenericEvent) bool {
			return e.Object.GetLabels()["kubernetes.io/hostname"] == nodeName
		},
		CreateFunc: func(e event.CreateEvent) bool {
			return e.Object.GetLabels()["kubernetes.io/hostname"] == nodeName
		},
		UpdateFunc: func(e event.UpdateEvent) bool {
			return e.ObjectNew.GetLabels()["kubernetes.io/hostname"] == nodeName
		},
		DeleteFunc: func(e event.DeleteEvent) bool {
			return e.Object.GetLabels()["kubernetes.io/hostname"] == nodeName
		},
	}
}

func isNodeSelected(selector *metav1.LabelSelector, nodeLabels map[string]string) (bool, error) {
	// Logic to check if this node is selected by the *Program object
	selectorTool, err := metav1.LabelSelectorAsSelector(selector)
	if err != nil {
		return false, fmt.Errorf("failed to parse nodeSelector: %v",
			err)
	}

	nodeLabelSet, err := labels.ConvertSelectorToLabelsMap(labels.FormatLabels(nodeLabels))
	if err != nil {
		return false, fmt.Errorf("failed to parse node labels : %v",
			err)
	}

	return selectorTool.Matches(nodeLabelSet), nil
}

func getInterfaces(interfaceSelector *bpfdiov1alpha1.InterfaceSelector, ourNode *v1.Node) ([]string, error) {
	var interfaces []string

	if interfaceSelector.Interfaces != nil {
		return *interfaceSelector.Interfaces, nil
	}

	if interfaceSelector.PrimaryNodeInterface != nil {
		nodeIface, err := bpfdagentinternal.GetPrimaryNodeInterface(ourNode)
		if err != nil {
			return nil, err
		}

		interfaces = append(interfaces, nodeIface)
		return interfaces, nil
	}

	return nil, fmt.Errorf("no interfaces selected")

}

// removeFinalizer removes the finalizer from the BpfProgram object if is applied,
// returning if the action resulted in a kube API update or not along with any
// errors.
func (r *ReconcilerCommon) removeFinalizer(ctx context.Context, o client.Object, finalizer string) bool {
	r.Logger.V(1).WithValues("object name", o.GetName()).Info("bpfProgram is deleted, don't load program, remove finalizer")

	changed := controllerutil.RemoveFinalizer(o, finalizer)
	if changed {
		err := r.Update(ctx, o)
		if err != nil {
			r.Logger.Error(err, "failed to remove bpfProgram Finalizer")
			return true
		}
	}

	return changed
}

// updateStatus updates the status of a BpfProgram object if needed, returning
// false if the status was already set for the given bpfProgram, meaning reconciliation
// may continue.
func (r *ReconcilerCommon) updateStatus(ctx context.Context, prog *bpfdiov1alpha1.BpfProgram, cond bpfdiov1alpha1.BpfProgramConditionType) bool {
	if prog.Status.Conditions != nil {
		// Get most recent condition
		recentIdx := len(prog.Status.Conditions) - 1

		// If the most recent condition is the same as input, just return.
		if prog.Status.Conditions[recentIdx].Type == string(cond) {
			return false
		} else {
			// Remove the input condition from the list if it exists (may not exist)
			// because the SetStatusCondition() doesn't append if it is already in the list.
			meta.RemoveStatusCondition(&prog.Status.Conditions, string(cond))
		}
	}

	meta.SetStatusCondition(&prog.Status.Conditions, cond.Condition())
	r.Logger.V(1).WithValues("bpfProgram", prog.Name, "condition", cond.Condition().Type).Info("Updating bpfProgram condition")
	if err := r.Status().Update(ctx, prog); err != nil {
		r.Logger.Error(err, "failed to set bpfProgram object status")
		return true
	}

	return true
}

func (r *ReconcilerCommon) getExistingBpfProgs(ctx context.Context, owner metav1.Object) (map[string]bpfdiov1alpha1.BpfProgram, error) {
	bpfProgramList := &bpfdiov1alpha1.BpfProgramList{}

	// Only list bpfPrograms for this *Program and the controller's node
	opts := []client.ListOption{
		client.MatchingLabels{internal.BpfProgramOwnerLabel: owner.GetName(), internal.K8sHostLabel: r.NodeName},
	}

	err := r.List(ctx, bpfProgramList, opts...)
	if err != nil {
		return nil, err
	}

	existingProgs := map[string]bpfdiov1alpha1.BpfProgram{}
	for _, bpfProg := range bpfProgramList.Items {
		existingProgs[bpfProg.GetName()] = bpfProg
	}

	return existingProgs, nil
}

// createBpfProgram moves some shared logic for building bpfProgram objects
// into a central location.
func (r *ReconcilerCommon) createBpfProgram(ctx context.Context,
	bpfProgramName string,
	finalizer string,
	owner metav1.Object,
	ownerType string,
	annotations map[string]string) (*bpfdiov1alpha1.BpfProgram, error) {
	bpfProg := &bpfdiov1alpha1.BpfProgram{
		ObjectMeta: metav1.ObjectMeta{
			Name:       bpfProgramName,
			Finalizers: []string{finalizer},
			Labels: map[string]string{internal.BpfProgramOwnerLabel: owner.GetName(),
				internal.K8sHostLabel: r.NodeName},
			Annotations: annotations,
		},
		Spec: bpfdiov1alpha1.BpfProgramSpec{
			Type: ownerType,
		},
		Status: bpfdiov1alpha1.BpfProgramStatus{Conditions: []metav1.Condition{}},
	}

	// Make the corresponding BpfProgramConfig the owner
	if err := ctrl.SetControllerReference(owner, bpfProg, r.Scheme); err != nil {
		return nil, fmt.Errorf("failed to bpfProgram object owner reference: %v", err)
	}

	return bpfProg, nil
}

// reconcileProgram is called by ALL *Program controllers, and contains much of
// the core logic for taking *Program objects, turning them into bpfProgram
// object(s), and ultimately telling the custom controller types to load real
// bpf programs on the node via bpfd. Additionally it acts as a central point for
// interacting with the K8s API. This function will exit if any action is taken
// against the K8s API. If the function returns a retry boolean and error, the
// reconcile will be retried based on a default 5 second interval if the retry
// boolean is set to `true`.
func reconcileProgram(ctx context.Context,
	rec bpfdReconciler,
	program client.Object,
	common *bpfdiov1alpha1.BpfProgramCommon,
	ourNode *v1.Node,
	programMap map[string]*gobpfd.ListResponse_ListResult) (bool, error) {

	// initialize reconciler state
	r := rec.getRecCommon()

	// Get program bytecode, source could be an OCI container image or filepath
	bytecode, err := bpfdagentinternal.GetBytecode(r.Client, &common.ByteCode)
	if err != nil {
		return false, fmt.Errorf("failed to process bytecode selector: %v", err)
	}

	// Determine which node local actions should be taken based on whether the node is selected
	// OR if the *Program is being deleted.
	isNodeSelected, err := isNodeSelected(&common.NodeSelector, ourNode.Labels)
	if err != nil {
		return false, fmt.Errorf("failed to check if node is selected: %v", err)
	}

	isBeingDeleted := !program.GetDeletionTimestamp().IsZero()

	// Query the K8s API to get a list of existing bpfPrograms for this *Program
	// on this node.
	existingPrograms, err := r.getExistingBpfProgs(ctx, program)
	if err != nil {
		return false, fmt.Errorf("failed to get existing bpfPrograms: %v", err)
	}

	// Generate the list of BpfPrograms for this *Program. This handles the one
	// *Program to many BpfPrograms (i.e. One *Program maps to multiple
	// interfaces because of PodSelector)
	expectedPrograms, err := rec.expectedBpfPrograms(ctx)
	if err != nil {
		return true, fmt.Errorf("failed to get expected bpfPrograms: %v", err)
	}

	// Determine if the MapOwnerSelector was set, and if so, see if the MapOwner
	// UUID can be found.
	mapOwnerStatus, err := ProcessMapOwnerParam(ctx, &common.MapOwnerSelector, r)
	if err != nil {
		return false, fmt.Errorf("failed to determine map owner: %v", err)
	}
	r.Logger.V(1).Info("ProcessMapOwnerParam",
		"isSet", mapOwnerStatus.isSet,
		"isFound", mapOwnerStatus.isFound,
		"isLoaded", mapOwnerStatus.isLoaded,
		"mapOwnerUuid", mapOwnerStatus.mapOwnerUuid)

	// multiplex signals into kubernetes API actions
	switch isBeingDeleted {
	// Deletion of a *Program takes a few steps if there's existing bpfPrograms:
	// 1. Reconcile the bpfProgram (take bpfd cleanup steps).
	// 2. Remove any finalizers from the bpfProgram Object.
	// 3. Update the condition on the bpfProgram to BpfProgCondUnloaded so the
	//    operator knows it's safe to remove the parent Program Object, which
	//	  is when the bpfProgram is automatically deleted by the owner-reference.
	case true:
		for _, prog := range existingPrograms {
			// Reconcile the bpfProgram if error write condition and exit with
			// retry.
			cond, err := rec.reconcileBpfdProgram(ctx,
				programMap,
				bytecode,
				&prog,
				isNodeSelected,
				isBeingDeleted,
				mapOwnerStatus,
			)
			if err != nil {
				r.updateStatus(ctx, &prog, cond)
				return true, fmt.Errorf("failed to delete bpfd program: %v", err)
			}

			updatedFinalizers := r.removeFinalizer(ctx, &prog, rec.getFinalizer())
			if updatedFinalizers {
				return false, nil
			}

			updatedStatus := r.updateStatus(ctx, &prog, cond)
			if updatedStatus {
				return false, nil
			}
		}
	// If the *Program isn't being deleted ALWAYS create the bpfPrograms
	// even if the node isn't selected
	case false:
		for _, expectedProg := range expectedPrograms.Items {
			prog, exists := existingPrograms[expectedProg.Name]
			if !exists {
				opts := client.CreateOptions{}
				r.Logger.Info("creating bpfProgram", "Name", expectedProg.Name, "Owner", program.GetName())
				if err := r.Create(ctx, &expectedProg, &opts); err != nil {
					return true, fmt.Errorf("failed to create bpfProgram object: %v", err)
				}
				existingPrograms[expectedProg.Name] = prog
				return false, nil
			}

			// bpfProgram Object exists go ahead and reconcile it, if there is
			// an error write condition and exit with retry.
			cond, err := rec.reconcileBpfdProgram(ctx,
				programMap,
				bytecode,
				&prog,
				isNodeSelected,
				isBeingDeleted,
				mapOwnerStatus,
			)
			if err != nil {
				r.updateStatus(ctx, &prog, cond)
				return true, fmt.Errorf("failed to reconcile bpfd program: %v", err)
			}

			// Make sure if we're not selected exit and write correct condition
			if cond == bpfdiov1alpha1.BpfProgCondNotSelected ||
				cond == bpfdiov1alpha1.BpfProgCondMapOwnerNotFound ||
				cond == bpfdiov1alpha1.BpfProgCondMapOwnerNotLoaded {
				// Write NodeNodeSelected status
				updatedStatus := r.updateStatus(ctx, &prog, cond)
				if updatedStatus {
					r.Logger.V(1).Info("Update condition from bpfd reconcile", "condition", cond)
					return false, nil
				}
			}

			// If bpfProgram Maps isn't up to date just update it and return
			if !reflect.DeepEqual(prog.Spec.Maps, r.expectedMaps) && len(r.expectedMaps) != 0 {
				r.Logger.V(1).Info("Updating bpfProgram Object", "Maps", r.expectedMaps, "bpfProgram", prog.Name)
				prog.Spec.Maps = r.expectedMaps
				if err := r.Update(ctx, &prog, &client.UpdateOptions{}); err != nil {
					return true, fmt.Errorf("failed to update bpfProgram's Programs: %v", err)
				}
				return false, nil
			}

			updatedStatus := r.updateStatus(ctx, &prog, cond)
			if updatedStatus {
				return false, nil
			}
		}
	}

	// nothing to do
	return false, nil
}

// MapOwnerParamStatus provides the output from a MapOwerSelector being parsed.
type MapOwnerParamStatus struct {
	isSet        bool
	isFound      bool
	isLoaded     bool
	mapOwnerUuid types.UID
}

// This function parses the MapOwnerSelector field from the BpfProgramCommon struct
// in the *Program CRDs. The field is a Label Selector. The labels should map to
// an eBPF Program that this eBPF Program wants to share maps with. If found, this
// function returns the UUID of the EBPF Program that owns the map to be shared.
// Found or not, this function also returns some flags (isSet, isFound, isLoaded)
// to help with the processing and set the proper condition on the BPF Object.
func ProcessMapOwnerParam(ctx context.Context,
	selector *metav1.LabelSelector,
	r *ReconcilerCommon) (*MapOwnerParamStatus, error) {
	mapOwnerStatus := &MapOwnerParamStatus{}

	// Parse the MapOwnerSelector label selector.
	mapOwnerSelectorMap, err := metav1.LabelSelectorAsMap(selector)
	if err != nil {
		mapOwnerStatus.isSet = true
		return mapOwnerStatus, fmt.Errorf("failed to parse MapOwnerSelector: %v", err)
	}

	// If no data was entered, just return with default values, all flags set to false.
	if len(mapOwnerSelectorMap) == 0 {
		return mapOwnerStatus, nil
	} else {
		mapOwnerStatus.isSet = true

		// Add the labels from the MapOwnerSelector to a map and add an additional
		// label to filter on just this node. Call K8s to find all the eBPF programs
		// that match this filter.
		var labelMap = make(client.MatchingLabels)
		labelMap[internal.K8sHostLabel] = r.NodeName
		for key, value := range mapOwnerSelectorMap {
			labelMap[key] = value
		}
		opts := []client.ListOption{labelMap}
		bpfProgramList := &bpfdiov1alpha1.BpfProgramList{}
		r.Logger.V(1).Info("MapOwner Labels:", "opts", opts)
		err := r.List(ctx, bpfProgramList, opts...)
		if err != nil {
			return mapOwnerStatus, err
		}

		// If no eBPF Programs were found, or more than one, then return.
		if len(bpfProgramList.Items) == 0 {
			return mapOwnerStatus, nil
		} else if len(bpfProgramList.Items) > 1 {
			return mapOwnerStatus, fmt.Errorf("MapOwnerSelector resolved to multiple pods")
		} else {
			mapOwnerStatus.isFound = true
			mapOwnerStatus.mapOwnerUuid = bpfProgramList.Items[0].GetUID()

			// Get most recent condition from the one eBPF Program and determine
			// if the eBPF Program is loaded or not.
			conLen := len(bpfProgramList.Items[0].Status.Conditions)
			if conLen > 0 &&
				bpfProgramList.Items[0].Status.Conditions[conLen-1].Type ==
					string(bpfdiov1alpha1.BpfProgCondLoaded) {
				mapOwnerStatus.isLoaded = true
			}

			return mapOwnerStatus, nil
		}
	}
}
