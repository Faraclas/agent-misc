/*
 * Hand-made fixture for the azdo-manuals tests, in the shape of microsoft/azure-devops-node-api's generated interfaces.
 */

import VSSInterfaces = require("../interfaces/common/VSSInterfaces");

/**
 * A gadget.
 */
export interface Gadget {
    /**
     * The gadget's id.
     */
    id?: number;
    /**
     * Kind of gadget.
     */
    kind?: GadgetKind;
    /**
     * Who owns it.
     */
    owner?: VSSInterfaces.IdentityRef;
}

/**
 * Kinds of gadgets.
 */
export enum GadgetKind {
    /**
     * A small gadget.
     */
    Small = 0,
    /**
     * A large gadget.
     */
    Large = 1,
}

/**
 * One revision of a gadget.
 */
export interface GadgetRevision extends Gadget {
    /**
     * Revision number.
     */
    rev?: number;
    /**
     * Values by field.
     */
    fields?: { [key: string] : any; };
}

/**
 * What a history includes.
 */
export interface HistoryCriteria {
    /**
     * Only this kind.
     */
    kind?: GadgetKind;
    /**
     * Include deleted revisions.
     */
    includeDeleted?: boolean;
    /**
     * Not sent flattened (an object).
     */
    owner?: VSSInterfaces.IdentityRef;
}

export interface Attachment {
    /**
     * Where it is.
     */
    url?: string;
}
