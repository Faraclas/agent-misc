/*
 * Hand-made fixture for the azdo-manuals tests, in the shape of microsoft/azure-devops-node-api's generated clients.
 */

import * as restm from 'typed-rest-client/RestClient';
import vsom = require('./VsoClient');
import basem = require('./ClientApiBases');
import AlphaInterfaces = require("./interfaces/AlphaInterfaces");
import VSSInterfaces = require("./interfaces/common/VSSInterfaces");
import TfsCoreInterfaces = require("./interfaces/CoreInterfaces");

export class AlphaApi extends basem.ClientApiBase {

    /**
     * List gadgets (Go has this one too; Go wins).
     *
     * @param {string} project - Project ID or project name
     * @param {number} top - How many.
     */
    public async getGadgets(
        project: string,
        top?: number
        ): Promise<AlphaInterfaces.Gadget[]> {

        return new Promise<AlphaInterfaces.Gadget[]>(async (resolve, reject) => {
            let routeValues: any = {
                project: project
            };

            let queryValues: any = {
                '$top': top,
            };

            try {
                let verData: vsom.ClientVersioningData = await this.vsoClient.getVersioningData(
                    "7.2-preview.3",
                    "alpha",
                    "11111111-0000-0000-0000-000000000002",
                    routeValues,
                    queryValues);

                let url: string = verData.requestUrl!;
                let options: restm.IRequestOptions = this.createRequestOptions('application/json',
                                                                                verData.apiVersion);

                let res: restm.IRestResponse<AlphaInterfaces.Gadget[]>;
                res = await this.rest.get<AlphaInterfaces.Gadget[]>(url, options);

                resolve(res.result);
            }
            catch (err) {
                reject(err);
            }
        });
    }

    /**
     * Get the history of a gadget.
     *
     * @param {string} project - Project ID or project name
     * @param {number} gadgetId - The gadget's id.
     * @param {AlphaInterfaces.HistoryCriteria} criteria - What to include.
     * @param {string[]} fields - Fields to return.
     * @param {Date} since - Changed since.
     */
    public async getGadgetHistory(
        project: string,
        gadgetId: number,
        criteria?: AlphaInterfaces.HistoryCriteria,
        fields?: string[],
        since?: Date
        ): Promise<AlphaInterfaces.GadgetRevision[]> {

        return new Promise<AlphaInterfaces.GadgetRevision[]>(async (resolve, reject) => {
            let routeValues: any = {
                project: project,
                gadgetId: gadgetId
            };

            let queryValues: any = {
                criteria: criteria,
                fields: fields && fields.join(","),
                since: since,
            };

            try {
                let verData: vsom.ClientVersioningData = await this.vsoClient.getVersioningData(
                    "7.2-preview.1",
                    "alpha",
                    "11111111-0000-0000-0000-000000000003",
                    routeValues,
                    queryValues);

                let url: string = verData.requestUrl!;
                let options: restm.IRequestOptions = this.createRequestOptions('application/json',
                                                                                verData.apiVersion);

                let res: restm.IRestResponse<AlphaInterfaces.GadgetRevision[]>;
                res = await this.rest.get<AlphaInterfaces.GadgetRevision[]>(url, options);

                resolve(res.result);
            }
            catch (err) {
                reject(err);
            }
        });
    }

    /**
     * Get filters of a kind for a repository.
     *
     * @param {string} project - Project ID or project name
     * @param {string} repository - The repository.
     */
    public async getBranchFilters(
        project: string,
        repository: string
        ): Promise<string[]> {

        return new Promise<string[]>(async (resolve, reject) => {
            let routeValues: any = {
                project: project,
                repository: repository,
                action: "Branches"
            };

            try {
                let verData: vsom.ClientVersioningData = await this.vsoClient.getVersioningData(
                    "7.2-preview.1",
                    "alpha",
                    "11111111-0000-0000-0000-000000000009",
                    routeValues);

                let url: string = verData.requestUrl!;
                let options: restm.IRequestOptions = this.createRequestOptions('application/json',
                                                                                verData.apiVersion);

                let res: restm.IRestResponse<string[]>;
                res = await this.rest.get<string[]>(url, options);

                resolve(res.result);
            }
            catch (err) {
                reject(err);
            }
        });
    }

    /**
     * Upload an attachment.
     *
     * @param {NodeJS.ReadableStream} contentStream - Content to upload
     * @param {string} project - Project ID or project name
     * @param {string} fileName - The file's name.
     */
    public async createAttachment(
        customHeaders: any,
        contentStream: NodeJS.ReadableStream,
        project: string,
        fileName: string
        ): Promise<AlphaInterfaces.Attachment> {

        return new Promise<AlphaInterfaces.Attachment>(async (resolve, reject) => {
            let routeValues: any = {
                project: project,
                fileName: fileName
            };

            customHeaders = customHeaders || {};
            customHeaders["Content-Type"] = "application/octet-stream";

            try {
                let verData: vsom.ClientVersioningData = await this.vsoClient.getVersioningData(
                    "7.2-preview.1",
                    "alpha",
                    "11111111-0000-0000-0000-000000000010",
                    routeValues);

                let url: string = verData.requestUrl!;

                let options: restm.IRequestOptions = this.createRequestOptions('application/json',
                                                                                verData.apiVersion);
                options.additionalHeaders = customHeaders;

                let res: restm.IRestResponse<AlphaInterfaces.Attachment>;
                res = await this.rest.uploadStream<AlphaInterfaces.Attachment>("PUT", url, contentStream, options);

                resolve(res.result);
            }
            catch (err) {
                reject(err);
            }
        });
    }

    /**
     * Update the parts of a gadget.
     *
     * @param {VSSInterfaces.JsonPatchDocument} patchDocument - Operations to apply.
     * @param {string} project - Project ID or project name
     * @param {number} gadgetId - The gadget's id.
     */
    public async updateGadgetParts(
        customHeaders: any,
        patchDocument: VSSInterfaces.JsonPatchDocument,
        project: string,
        gadgetId: number
        ): Promise<void> {

        return new Promise<void>(async (resolve, reject) => {
            let routeValues: any = {
                project: project,
                gadgetId: gadgetId
            };

            customHeaders = customHeaders || {};
            customHeaders["Content-Type"] = "application/json-patch+json";

            try {
                let verData: vsom.ClientVersioningData = await this.vsoClient.getVersioningData(
                    "7.2-preview.1",
                    "alpha",
                    "11111111-0000-0000-0000-000000000011",
                    routeValues);

                let url: string = verData.requestUrl!;
                let options: restm.IRequestOptions = this.createRequestOptions('application/json',
                                                                                verData.apiVersion);
                options.additionalHeaders = customHeaders;

                let res: restm.IRestResponse<void>;
                res = await this.rest.update<void>(url, patchDocument, options);

                resolve(res.result);
            }
            catch (err) {
                reject(err);
            }
        });
    }

    /**
     * Get a gadget's log as text.
     *
     * @param {string} project - Project ID or project name
     * @param {number} logId - The log's id.
     */
    public async getGadgetLog(
        project: string,
        logId: number
        ): Promise<NodeJS.ReadableStream> {

        return new Promise<NodeJS.ReadableStream>(async (resolve, reject) => {
            let routeValues: any = {
                project: project,
                logId: logId
            };

            try {
                let verData: vsom.ClientVersioningData = await this.vsoClient.getVersioningData(
                    "7.2-preview.1",
                    "alpha",
                    "11111111-0000-0000-0000-000000000012",
                    routeValues);

                let url: string = verData.requestUrl!;

                let apiVersion: string = verData.apiVersion!;
                let accept: string = this.createAcceptHeader("text/plain", apiVersion);
                resolve((await this.http.get(url, { "Accept": accept })).message);
            }
            catch (err) {
                reject(err);
            }
        });
    }

    /**
     * Get a team's board settings.
     *
     * @param {TfsCoreInterfaces.TeamContext} teamContext - The team context for the operation
     */
    public async getBoardSettings(
        teamContext: TfsCoreInterfaces.TeamContext
        ): Promise<AlphaInterfaces.Gadget> {

        return new Promise<AlphaInterfaces.Gadget>(async (resolve, reject) => {
            let project = null;
            let team = null;
            if (teamContext) {
                project = teamContext.projectId || teamContext.project;
                team = teamContext.teamId || teamContext.team;
            }

            let routeValues: any = {
                project: project,
                team: team
            };

            try {
                let verData: vsom.ClientVersioningData = await this.vsoClient.getVersioningData(
                    "7.2-preview.1",
                    "alpha",
                    "11111111-0000-0000-0000-000000000013",
                    routeValues);

                let url: string = verData.requestUrl!;
                let options: restm.IRequestOptions = this.createRequestOptions('application/json',
                                                                                verData.apiVersion);

                let res: restm.IRestResponse<AlphaInterfaces.Gadget>;
                res = await this.rest.get<AlphaInterfaces.Gadget>(url, options);

                resolve(res.result);
            }
            catch (err) {
                reject(err);
            }
        });
    }
}
