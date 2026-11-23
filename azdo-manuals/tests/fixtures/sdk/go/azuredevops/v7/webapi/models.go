// Hand-made fixture for the azdo-manuals tests, in the shape of microsoft/azure-devops-go-api's generated models.

package webapi

// An identity.
type IdentityRef struct {
	// Its display name.
	DisplayName *string `json:"displayName,omitempty"`
	// Its id.
	Id *string `json:"id,omitempty"`
}

// What a connection includes.
type ConnectOptions string

type connectOptionsValuesType struct {
	None            ConnectOptions
	IncludeServices ConnectOptions
}

var ConnectOptionsValues = connectOptionsValuesType{
	// Nothing more.
	None: "none",
	// The services.
	IncludeServices: "includeServices",
}
