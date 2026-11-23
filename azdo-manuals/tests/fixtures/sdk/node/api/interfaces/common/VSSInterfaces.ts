/*
 * Hand-made fixture for the azdo-manuals tests, in the shape of microsoft/azure-devops-node-api's generated interfaces.
 */

/**
 * An identity.
 */
export interface IdentityRef {
    /**
     * Its display name.
     */
    displayName?: string;
    /**
     * Its id.
     */
    id?: string;
}

export interface JsonPatchOperation {
    /**
     * The operation.
     */
    op?: string;
    /**
     * The path.
     */
    path?: string;
    value?: any;
}

export interface JsonPatchDocument {
}
