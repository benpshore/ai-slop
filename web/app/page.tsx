import {requireChatGPTUser} from './chatgpt-auth';
import Workspace from './workspace';
export const dynamic='force-dynamic';
export default async function Home(){const user=await requireChatGPTUser('/');return <Workspace key={user.userId} userId={user.userId}/>;}
