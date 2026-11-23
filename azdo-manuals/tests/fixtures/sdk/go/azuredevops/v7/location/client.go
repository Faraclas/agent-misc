// Hand-made fixture for the azdo-manuals tests, in the shape of microsoft/azure-devops-go-api's generated clients.

package location

import (
	"context"
	"net/http"
	"net/url"
	"strconv"
	"uuid"
)

type ClientImpl struct {
	Client azuredevops.Client
}

// [Preview API] Who is calling, and from where.
func (client *ClientImpl) GetConnectionData(ctx context.Context, args GetConnectionDataArgs) (*ConnectionData, error) {
	queryParams := url.Values{}
	if args.ConnectOptions != nil {
		queryParams.Add("connectOptions", string(*args.ConnectOptions))
	}
	if args.LastChangeId64 != nil {
		queryParams.Add("lastChangeId64", strconv.FormatUint(*args.LastChangeId64, 10))
	}
	locationId, _ := uuid.Parse("11111111-0000-0000-0000-000000000004")
	resp, err := client.Client.Send(ctx, http.MethodGet, locationId, "7.1-preview.1", nil, queryParams, nil, "", "application/json", nil)
	if err != nil {
		return nil, err
	}

	var responseValue ConnectionData
	err = client.Client.UnmarshalBody(resp, &responseValue)
	return &responseValue, err
}

// Arguments for the GetConnectionData function
type GetConnectionDataArgs struct {
	// (optional)
	ConnectOptions *webapi.ConnectOptions
	// (optional) The last change seen.
	LastChangeId64 *uint64
}
