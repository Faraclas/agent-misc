// Hand-made fixture for the azdo-manuals tests, in the shape of microsoft/azure-devops-go-api's generated clients.

package alpha

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/url"
	"strconv"
	"strings"

	"github.com/google/uuid"
	"github.com/microsoft/azure-devops-go-api/azuredevops/v7"
)

type ClientImpl struct {
	Client azuredevops.Client
}

// [Preview API] Create a gadget.
func (client *ClientImpl) CreateGadget(ctx context.Context, args CreateGadgetArgs) (*Gadget, error) {
	if args.Gadget == nil {
		return nil, &azuredevops.ArgumentNilError{ArgumentName: "args.Gadget"}
	}
	routeValues := make(map[string]string)
	if args.Project == nil || *args.Project == "" {
		return nil, &azuredevops.ArgumentNilOrEmptyError{ArgumentName: "args.Project"}
	}
	routeValues["project"] = *args.Project

	body, marshalErr := json.Marshal(*args.Gadget)
	if marshalErr != nil {
		return nil, marshalErr
	}
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000002")
	resp, err := client.Client.Send(ctx, http.MethodPost, locationId, "7.1-preview.1", routeValues, nil, bytes.NewReader(body), "application/json", "application/json", nil)
	if err != nil {
		return nil, err
	}

	var responseValue Gadget
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the CreateGadget function
type CreateGadgetArgs struct {
	// (required) The gadget to create.
	Gadget *Gadget
	// (required) Project ID or project name
	Project *string
}

// [Preview API] Delete a gadget.
func (client *ClientImpl) DeleteGadget(ctx context.Context, args DeleteGadgetArgs) error {
	routeValues := make(map[string]string)
	routeValues["project"] = *args.Project
	routeValues["gadgetId"] = strconv.Itoa(*args.GadgetId)

	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000002")
	_, err := client.Client.Send(ctx, http.MethodDelete, locationId, "7.1-preview.1", routeValues, nil, nil, "", "application/json", nil)
	return err
}

// Arguments for the DeleteGadget function
type DeleteGadgetArgs struct {
	// (required) Project ID or project name
	Project *string
	// (required) The gadget's id.
	GadgetId *int
}

// [Preview API] Get a gadget.
func (client *ClientImpl) GetGadget(ctx context.Context, args GetGadgetArgs) (*Gadget, error) {
	routeValues := make(map[string]string)
	if args.Project != nil && *args.Project != "" {
		routeValues["project"] = *args.Project
	}
	if args.GadgetId == nil {
		return nil, &azuredevops.ArgumentNilError{ArgumentName: "args.GadgetId"}
	}
	routeValues["gadgetId"] = strconv.Itoa(*args.GadgetId)

	queryParams := url.Values{}
	if args.IncludeParts != nil {
		queryParams.Add("includeParts", strconv.FormatBool(*args.IncludeParts))
	}
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000002")
	resp, err := client.Client.Send(ctx, http.MethodGet, locationId, "7.1-preview.1", routeValues, queryParams, nil, "", "application/json", nil)
	if err != nil {
		return nil, err
	}

	var responseValue Gadget
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the GetGadget function
type GetGadgetArgs struct {
	// (optional) Project ID or project name
	Project *string
	// (required) The gadget's id.
	GadgetId *int
	// (optional) Include the gadget's parts.
	IncludeParts *bool
}

// [Preview API] List gadgets.
func (client *ClientImpl) GetGadgets(ctx context.Context, args GetGadgetsArgs) (*[]Gadget, error) {
	routeValues := make(map[string]string)
	routeValues["project"] = *args.Project

	queryParams := url.Values{}
	if args.Ids != nil {
		var stringList []string
		for _, item := range *args.Ids {
			stringList = append(stringList, strconv.Itoa(item))
		}
		listAsString := strings.Join((stringList)[:], ",")
		queryParams.Add("ids", listAsString)
	}
	if args.SearchCriteria != nil {
		if args.SearchCriteria.Kind != nil {
			queryParams.Add("searchCriteria.kind", string(*args.SearchCriteria.Kind))
		}
	}
	if args.Since != nil {
		queryParams.Add("since", (*args.Since).AsQueryParameter())
	}
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000002")
	resp, err := client.Client.Send(ctx, http.MethodGet, locationId, "7.1-preview.1", routeValues, queryParams, nil, "", "application/json", nil)
	if err != nil {
		return nil, err
	}

	var responseValue []Gadget
	err = client.Client.UnmarshalCollectionBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the GetGadgets function
type GetGadgetsArgs struct {
	// (required) Project ID or project name
	Project *string
	// (optional) Only these gadgets.
	Ids *[]int
	// (optional)
	SearchCriteria *GadgetSearchCriteria
	// (optional) Changed since.
	Since *azuredevops.Time
}

// Update a gadget.
func (client *ClientImpl) UpdateGadget(ctx context.Context, args UpdateGadgetArgs) (*Gadget, error) {
	routeValues := make(map[string]string)
	routeValues["project"] = *args.Project
	routeValues["gadgetId"] = strconv.Itoa(*args.GadgetId)
	additionalHeaders := make(map[string]string)
	if args.Version != nil {
		additionalHeaders["If-Match"] = *args.Version
	}
	body, marshalErr := json.Marshal(*args.Gadget)
	if marshalErr != nil {
		return nil, marshalErr
	}
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000002")
	resp, err := client.Client.Send(ctx, http.MethodPatch, locationId, "7.1-preview.1", routeValues, nil, bytes.NewReader(body), "application/json", "application/json", additionalHeaders)
	if err != nil {
		return nil, err
	}

	var responseValue Gadget
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the UpdateGadget function
type UpdateGadgetArgs struct {
	// (required) The new gadget.
	Gadget *Gadget
	// (required) Project ID or project name
	Project *string
	// (required) The gadget's id.
	GadgetId *int
	// (optional) The version to replace.
	Version *string
}

// Get a widget (the spec has this method on this endpoint).
func (client *ClientImpl) GetWidgetById(ctx context.Context, args GetWidgetByIdArgs) (*Gadget, error) {
	routeValues := make(map[string]string)
	routeValues["project"] = *args.Project
	routeValues["widgetId"] = *args.WidgetId

	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000001")
	resp, err := client.Client.Send(ctx, http.MethodGet, locationId, "7.1", routeValues, nil, nil, "", "application/json", nil)
	if err != nil {
		return nil, err
	}

	var responseValue Gadget
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the GetWidgetById function
type GetWidgetByIdArgs struct {
	// (required) Project ID or project name
	Project *string
	// (required) The widget's id.
	WidgetId *string
}

// Get the widget archive as a zip.
func (client *ClientImpl) GetWidgetArchive(ctx context.Context, args GetWidgetArchiveArgs) (io.ReadCloser, error) {
	routeValues := make(map[string]string)
	routeValues["project"] = *args.Project
	routeValues["widgetId"] = *args.WidgetId

	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000001")
	resp, err := client.Client.Send(ctx, http.MethodPut, locationId, "7.1", routeValues, nil, nil, "", "application/zip", nil)
	if err != nil {
		return nil, err
	}

	return resp.Body, err
}

// Arguments for the GetWidgetArchive function
type GetWidgetArchiveArgs struct {
	// (required) Project ID or project name
	Project *string
	// (required) The widget's id.
	WidgetId *string
}

// A method on an endpoint that starts after the manuals' API version.
func (client *ClientImpl) GetFuture(ctx context.Context, args GetFutureArgs) (*Gadget, error) {
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000007")
	resp, err := client.Client.Send(ctx, http.MethodGet, locationId, "7.2-preview.1", nil, nil, nil, "", "application/json", nil)
	var responseValue Gadget
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the GetFuture function
type GetFutureArgs struct {
}

// A method whose endpoint the organization does not serve.
func (client *ClientImpl) GetOrphan(ctx context.Context, args GetOrphanArgs) (*Gadget, error) {
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000099")
	resp, err := client.Client.Send(ctx, http.MethodGet, locationId, "7.1", nil, nil, nil, "", "application/json", nil)
	var responseValue Gadget
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the GetOrphan function
type GetOrphanArgs struct {
}

// Telemetry, in an excluded route area.
func (client *ClientImpl) PublishEvents(ctx context.Context, args PublishEventsArgs) error {
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000005")
	_, err := client.Client.Send(ctx, http.MethodPost, locationId, "7.1", nil, nil, nil, "", "application/json", nil)
	return err
}

// Arguments for the PublishEvents function
type PublishEventsArgs struct {
}

// In a route area no manual takes.
func (client *ClientImpl) GetMystery(ctx context.Context, args GetMysteryArgs) (*Gadget, error) {
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000006")
	resp, err := client.Client.Send(ctx, http.MethodGet, locationId, "7.1", nil, nil, nil, "", "application/json", nil)
	var responseValue Gadget
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the GetMystery function
type GetMysteryArgs struct {
}
