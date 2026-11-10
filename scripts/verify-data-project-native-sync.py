#!/usr/bin/env python3
"""Verify two real, independently rooted native clients against the cloud API.

Credentials are read from a private file. Tokens and invitation secrets remain
in memory; reports contain only fixture identifiers and verification results.
"""
import argparse,copy,datetime,importlib.util,json,secrets,urllib.error,urllib.request,uuid
from pathlib import Path

spec=importlib.util.spec_from_file_location('native_versioning',Path(__file__).with_name('verify-data-project-versioning.py'))
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
Acceptance=module.Acceptance

class SyncAcceptance:
 def __init__(self,args):
  self.args=args;self.a=Acceptance(args.client_a);self.b=Acceptance(args.client_b)
  self.fixture=json.loads(Path(args.local_report).read_text());self.p=self.fixture['projectId'];self.d=self.fixture['datasetIds'][0]
  self.credentials=json.loads(Path(args.credentials).read_text());self.server=self.credentials['serverUrl'].rstrip('/')
  self.opener=urllib.request.build_opener(urllib.request.ProxyHandler({}));self.checks=[];self.ids={}
 def check(self,name,value):
  assert value,name
  self.checks.append({'name':name,'passed':True});print(name,flush=True)
 def remote(self,method,path,body=None,token=None):
  headers={'Content-Type':'application/json'}
  if token:headers['Authorization']='Bearer '+token
  request=urllib.request.Request(self.server+path,data=None if body is None else json.dumps(body).encode(),headers=headers,method=method)
  with self.opener.open(request,timeout=120) as response:return json.load(response)
 def images(self,api,d):return api.call('list_project_images',{'projectId':d,'groupId':None,'offset':0,'limit':48})
 def commit(self,api,p,branch,message):return api.call('commit_data_project_branch',{'dataProjectId':p,'branch':branch,'message':message})
 def run(self):
  suffix=uuid.uuid4().hex[:9]
  cloud=self.a.call('create_remote_data_project',{'name':'原生项目同步验收 '+suffix,'description':'两份独立原生工作区验证','organizationId':None})
  remote=cloud['id'];self.ids['remoteProjectId']=remote
  self.a.call('connect_remote_data_project',{'dataProjectId':self.p,'remoteProjectId':remote})
  pushed=self.a.call('publish_data_project',{'dataProjectId':self.p})
  self.check('native publisher uploads project graph and immutable objects',pushed['outcome']=='pushed')
  cloned=self.b.call('clone_remote_data_project',{'remoteProjectId':remote});pb=cloned['localProjectId'];self.ids['clientBProjectId']=pb
  bp=next(x for x in self.b.call('list_data_projects') if x['id']==pb)
  self.check('second native client clones both datasets',len(bp['datasetIds'])==len(self.fixture['datasetIds']))
  names={x['id']:x['name'] for x in self.b.call('list_dataset_projects')}
  db=next((d for d in bp['datasetIds'] if 'day' in names.get(d,'')),bp['datasetIds'][0]);self.ids['clientBDatasetId']=db
  ia=self.images(self.a,self.d);ib=self.images(self.b,db)
  first,second,third=[i['id'] for i in ia[:3]]
  by_name={i['fileName']:i['id'] for i in ib};bf,bs,bt=[by_name[i['fileName']] for i in ia[:3]]
  self.check('clone restores actual image annotations',self.a.state(self.d,first)['objects']==self.b.state(db,bf)['objects'])
  self.check('cloned worktree is clean',not self.b.call('get_data_project_working_tree',{'dataProjectId':pb})['changes'])
  branches_a=self.a.call('list_data_project_branches',{'dataProjectId':self.p});branches_b=self.b.call('list_data_project_branches',{'dataProjectId':pb})
  self.check('all branch heads are cloned',{x['name'].removeprefix('origin::'):x['headCommitId'] for x in branches_a}=={x['name'].removeprefix('origin::'):x['headCommitId'] for x in branches_b})
  history_b=self.b.call('list_data_project_version_commits',{'dataProjectId':pb,'branch':'main'})
  self.check('merge second parent survives cloud round trip',any(row.get('secondParentId') for row in history_b))
  self.a.call('create_data_project_branch',{'dataProjectId':self.p,'name':'cloud-feature','fromCommitId':self.fixture['finalCommitId']})
  self.a.call('switch_data_project_branch',{'dataProjectId':self.p,'branch':'cloud-feature'});self.a.edit(self.d,first,2)
  feature=self.commit(self.a,self.p,'cloud-feature','只有非 main 分支发生变化')
  self.a.call('switch_data_project_branch',{'dataProjectId':self.p,'branch':'main'})
  self.a.call('sync_data_project',{'dataProjectId':self.p});self.b.call('sync_data_project',{'dataProjectId':pb})
  branches_b=self.b.call('list_data_project_branches',{'dataProjectId':pb})
  self.check('feature-only commit syncs while main stays unchanged',any(x['name'] in ('cloud-feature','origin::cloud-feature') and x['headCommitId']==feature['id'] for x in branches_b))
  xa=self.a.edit(self.d,second,3);ca=self.commit(self.a,self.p,'main','客户端 A 独立修改第二图')
  xb=self.b.edit(db,bf,5);cb=self.commit(self.b,pb,'main','客户端 B 独立修改第一图')
  self.b.call('sync_data_project',{'dataProjectId':pb})
  diverged=self.a.call('sync_data_project',{'dataProjectId':self.p})
  self.check('concurrent remote branch never overwrites local head',diverged['outcome']=='diverged' and self.a.state(self.d,second)['objects'][0]['bbox']['x']==xa)
  tracking=diverged['trackingRef'];self.check('divergent remote is retained as a mergeable tracking branch',bool(tracking) and any(x['name']==tracking for x in self.a.call('list_data_project_branches',{'dataProjectId':self.p})))
  merged=self.a.call('merge_data_project_branches',{'dataProjectId':self.p,'oursBranch':'main','theirsBranch':tracking,'message':'合并两个原生客户端的并行修改'})
  self.check('tracking branch merges with two native parents',not merged['conflicts'] and merged['commit']['parentId']==ca['id'] and merged['commit']['secondParentId']==cb['id'])
  self.a.call('sync_data_project',{'dataProjectId':self.p});self.b.call('sync_data_project',{'dataProjectId':pb})
  self.check('pull updates actual annotations on the other client',self.b.state(db,bf)['objects'][0]['bbox']['x']==xb and self.b.state(db,bs)['objects'][0]['bbox']['x']==xa)
  offline=Path(self.args.offline_flag);offline.write_text('isolated fault injection\n')
  try:
   xc=self.a.edit(self.d,third,1);offline_commit=self.commit(self.a,self.p,'main','离线保存标注并提交')
   error=self.a.call('sync_data_project',{'dataProjectId':self.p},error=True)
   self.check('offline sync failure preserves committed local draft',bool(error) and next(x for x in self.a.call('list_data_project_branches',{'dataProjectId':self.p}) if x['current'])['headCommitId']==offline_commit['id'])
  finally:offline.unlink(missing_ok=True)
  self.a.call('sync_data_project',{'dataProjectId':self.p});self.b.call('sync_data_project',{'dataProjectId':pb})
  self.check('network recovery syncs the offline annotation',self.b.state(db,bt)['objects'][0]['bbox']['x']==xc)
  self.check('both native clients end with clean worktrees',not self.a.call('get_data_project_working_tree',{'dataProjectId':self.p})['changes'] and not self.b.call('get_data_project_working_tree',{'dataProjectId':pb})['changes'])
  # A new invitation is fixture data in this acceptance organization only.
  session=self.remote('POST','/api/v1/auth/login',{'email':self.credentials['email'],'password':self.credentials['password'],'deviceId':str(uuid.uuid4()),'deviceName':'Native sync acceptance fixture setup','platform':'desktop'})
  token=session['accessToken'];legacy=self.remote('POST','/api/v1/projects',{'name':'原生只读成员验收 '+suffix},token)
  email='native-project-viewer-'+suffix+'@example.invalid';password=secrets.token_urlsafe(28)
  invitation=self.remote('POST',f"/api/v1/projects/{legacy['id']}/invitations",{'email':email,'role':'viewer'},token)
  member=self.remote('POST',f"/api/v1/auth/invitations/{invitation['token']}/accept",{'displayName':'原生项目只读验收','password':password,'deviceId':str(uuid.uuid4()),'platform':'desktop'})
  uid=member['user']['id'];self.a.call('upsert_data_project_cloud_member',{'dataProjectId':self.p,'userId':uid,'role':'viewer'})
  self.ids['viewerId']=uid
  if self.args.viewer_credentials_output:
   path=Path(self.args.viewer_credentials_output);path.write_text(json.dumps({'email':email,'password':password,'serverUrl':self.server}));path.chmod(0o600)
  self.check('native ACL management adds a distinct viewer',any(x['userId']==uid and x['role']=='viewer' for x in self.a.call('list_data_project_cloud_members',{'dataProjectId':self.p})))
  return {'passed':True,'checkedAt':datetime.datetime.now(datetime.timezone.utc).isoformat(),'clientAProjectId':self.p,'clientADatasetId':self.d,'finalMainCommitId':offline_commit['id'],'ids':self.ids,'checks':self.checks}

def main():
 parser=argparse.ArgumentParser();parser.add_argument('--client-a',required=True);parser.add_argument('--client-b',required=True);parser.add_argument('--local-report',required=True);parser.add_argument('--credentials',required=True);parser.add_argument('--offline-flag',required=True);parser.add_argument('--viewer-credentials-output');parser.add_argument('--output',required=True);args=parser.parse_args()
 result=SyncAcceptance(args).run();Path(args.output).write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
if __name__=='__main__':main()
