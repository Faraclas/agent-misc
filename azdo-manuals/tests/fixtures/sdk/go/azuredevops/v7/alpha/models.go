// Hand-made fixture for the azdo-manuals tests, in the shape of microsoft/azure-devops-go-api's generated models.

package alpha

import (
	"github.com/microsoft/azure-devops-go-api/azuredevops/v7"
	"github.com/microsoft/azure-devops-go-api/azuredevops/v7/webapi"
)

// A gadget.
type Gadget struct {
	// The gadget's id.
	Id *int `json:"id,omitempty"`
	// Kind of gadget.
	Kind *GadgetKind `json:"kind,omitempty"`
	// Who owns it.
	Owner *webapi.IdentityRef `json:"owner,omitempty"`
	// Its parts.
	Parts *[]GadgetPart `json:"parts,omitempty"`
	// Free-form labels.
	Labels *map[string]string `json:"labels,omitempty"`
	Url    *string            `json:"url,omitempty"`
}

// Kinds of gadgets.
type GadgetKind string

type gadgetKindValuesType struct {
	Small GadgetKind
	Large GadgetKind
}

var GadgetKindValues = gadgetKindValuesType{
	// A small gadget.
	Small: "small",
	// A large gadget.
	Large: "large",
}

// A part of a gadget.
type GadgetPart struct {
	// Part name.
	Name *string `json:"name,omitempty"`
	// Relative weight.
	Weight *float64 `json:"weight,omitempty"`
}

type GadgetSearchCriteria struct {
	// Only gadgets of this kind.
	Kind *GadgetKind `json:"kind,omitempty"`
}
