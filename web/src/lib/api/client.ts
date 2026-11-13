// The backend contract (D50): the `Api` type every page calls through `api()`. Each method's doc comment (in its
// area's `<Area>Api` type) names the unversioned `/api/<resource>` route agent-api serves. The app runs on
// `createLiveClient` (`live-client.ts`) only — the methods listed in `http/routes.ts` over HTTP (`http-client.ts`, the
// only file that may call fetch( … ); EventSource( … ) for the console/chat feeds) (tests/unit/guard.test.ts).
import type { CoreApi } from './areas/core';
import type { RuntimeApi } from './areas/runtime';
import type { TenantApi } from './areas/tenant';
import type { PlatformApi } from './areas/platform';
import type { ModelApi } from './areas/model';
import type { ConnectorsApi } from './areas/connectors';
import type { ReleaseApi } from './areas/release';
import type { JourneyApi } from './areas/journey';
import type { OntologyApi } from './areas/ontology';
import type { LiveApi } from './areas/live';
import type { RetrievalApi } from './areas/retrieval';
import type { GroundingApi } from './areas/grounding';
import type { InteractionApi } from './areas/interaction';
import type { ConversationsApi } from './areas/conversations';
import type { PkApi } from './areas/pk';
import type { SupportApi } from './areas/support';
import type { StatusApi } from './areas/status';
import type { IamApi } from './areas/iam';
import type { McpTasksApi } from './areas/mcp-tasks';

export type Api = CoreApi &
	RuntimeApi &
	TenantApi &
	PlatformApi &
	ModelApi &
	ConnectorsApi &
	ReleaseApi &
	JourneyApi &
	OntologyApi &
	LiveApi &
	RetrievalApi &
	GroundingApi &
	InteractionApi &
	ConversationsApi &
	PkApi &
	SupportApi &
	StatusApi &
	IamApi &
	McpTasksApi;
