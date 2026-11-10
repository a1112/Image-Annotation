#!/usr/bin/env python3
"""Exercise the actual native image/annotation API in an isolated workspace.

The native process must opt in with IMAGE_ANNOTATION_DATA_DIR. No existing
user datasets or external imported source files are modified by this runner.
"""
import argparse,copy,datetime,hashlib,json,urllib.request,uuid
from pathlib import Path

class Acceptance:
    def __init__(self,base):
        self.base=base.rstrip('/')
        self.client=urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.checks=[]
    def call(self,command,args=None,error=False):
        req=urllib.request.Request(self.base+'/api/invoke/'+command,data=json.dumps(args or {}).encode(),headers={'Content-Type':'application/json'},method='POST')
        try:
            with self.client.open(req,timeout=180) as response: result=json.load(response)
        except urllib.error.HTTPError as exc:
            result=json.loads(exc.read())
        if error:
            assert result.get('ok') is False,(command,'expected rejection')
            return result.get('error','')
        if result.get('ok') is not True:raise RuntimeError(command+': '+str(result.get('error')))
        return result['data']
    def check(self,name,condition):
        assert condition,name
        self.checks.append({'name':name,'passed':True})
        print(name,flush=True)
    def state(self,dataset,image):
        return self.call('get_image_annotation_state',{'projectId':dataset,'imageId':image})
    def edit(self,dataset,image,delta):
        state=self.state(dataset,image)
        objects=copy.deepcopy(state['objects'])
        assert objects and objects[0].get('bbox'),'fixture should include a box'
        objects[0]['bbox']['x']+=delta
        self.call('save_image_annotations',{'projectId':dataset,'imageId':image,'revision':state['revision'],'objects':objects})
        return objects[0]['bbox']['x']
    def run(self):
        suffix=uuid.uuid4().hex[:9]
        p=self.call('create_data_project',{'name':'数据版本完整验收 '+suffix,'description':'隔离工作区中的真实原生 API 验收'})['id']
        datasets=[self.call('create_dataset_project',{'name':'版本验收 '+suffix+' '+name,'datasetType':'yolo-detect','demoTemplate':'demo-bbox'}) for name in ['day','night']]
        for dataset in datasets:self.call('assign_dataset_to_data_project',{'dataProjectId':p,'datasetId':dataset['id']})
        d=datasets[0]['id']; images=self.call('list_project_images',{'projectId':d,'groupId':None,'offset':0,'limit':48})
        first,second,third=[row['id'] for row in images[:3]]
        originals={}
        for dataset in datasets:
            for image in self.call('list_project_images',{'projectId':dataset['id'],'groupId':None,'offset':0,'limit':48}):
                path=self.call('get_file_asset_path',{'projectId':dataset['id'],'imageId':image['id']})
                originals[path]=hashlib.sha256(Path(path).read_bytes()).hexdigest()
        self.check('project contains two datasets',len(next(x for x in self.call('list_data_projects') if x['id']==p)['datasetIds'])==2)
        self.call('list_data_project_branches',{'dataProjectId':p})
        initial=self.call('commit_data_project_branch',{'dataProjectId':p,'branch':'main','message':'首次收录两个数据集'})
        base_x=self.state(d,first)['objects'][0]['bbox']['x']; base_second=self.state(d,second)['objects'][0]['bbox']['x']
        self.call('create_data_project_branch',{'dataProjectId':p,'name':'feature','fromCommitId':initial['id']})
        self.call('switch_data_project_branch',{'dataProjectId':p,'branch':'feature'})
        feature_x=self.edit(d,first,4)
        feature=self.call('commit_data_project_branch',{'dataProjectId':p,'branch':'feature','message':'分支修改第一张图片'})
        self.call('switch_data_project_branch',{'dataProjectId':p,'branch':'main'})
        self.check('switch restores actual annotation API',self.state(d,first)['objects'][0]['bbox']['x']==base_x)
        main_second=self.edit(d,second,5)
        self.call('commit_data_project_branch',{'dataProjectId':p,'branch':'main','message':'主分支修改第二张图片'})
        merge=self.call('merge_data_project_branches',{'dataProjectId':p,'oursBranch':'main','theirsBranch':'feature','message':'合并独立图像修改'})
        self.check('independent edits merge with two parents',bool(merge['commit']) and bool(merge['commit'].get('secondParentId')) and not merge['conflicts'])
        self.check('merge restores both edits to annotation workbench',self.state(d,first)['objects'][0]['bbox']['x']==feature_x and self.state(d,second)['objects'][0]['bbox']['x']==main_second)
        self.check('merged working tree is clean',not self.call('get_data_project_working_tree',{'dataProjectId':p})['changes'])
        self.call('create_data_project_branch',{'dataProjectId':p,'name':'conflict','fromCommitId':merge['commit']['id']})
        self.call('switch_data_project_branch',{'dataProjectId':p,'branch':'conflict'})
        self.edit(d,first,6)
        self.call('commit_data_project_branch',{'dataProjectId':p,'branch':'conflict','message':'冲突分支修改同一图像'})
        self.call('switch_data_project_branch',{'dataProjectId':p,'branch':'main'})
        ours=self.edit(d,first,8)
        self.call('commit_data_project_branch',{'dataProjectId':p,'branch':'main','message':'主分支修改同一图像'})
        conflict=self.call('merge_data_project_branches',{'dataProjectId':p,'oursBranch':'main','theirsBranch':'conflict','message':'显式解决冲突'})
        self.check('conflict retains base ours theirs without overwriting worktree',bool(conflict['sessionId']) and bool(conflict['conflicts']) and self.state(d,first)['objects'][0]['bbox']['x']==ours)
        sid=conflict['sessionId']
        for item in list(conflict['conflicts']):
            conflict=self.call('resolve_data_project_merge',{'dataProjectId':p,'sessionId':sid,'conflictKey':item['key'],'resolution':'ours'})
        self.check('explicit resolution produces merge commit',bool(conflict['commit']) and not conflict['conflicts'] and self.state(d,first)['objects'][0]['bbox']['x']==ours)
        reverted=self.call('revert_data_project_commit',{'dataProjectId':p,'branch':'main','commitId':initial['id'],'message':'还原初始版本并保留历史'})
        self.check('revert creates a new commit and restores actual labels',reverted['id']!=initial['id'] and self.state(d,first)['objects'][0]['bbox']['x']==base_x and self.state(d,second)['objects'][0]['bbox']['x']==base_second)
        history=self.call('list_data_project_version_commits',{'dataProjectId':p,'branch':'main'})
        self.check('revert preserves earlier history',len(history)>=6 and any(row['id']==initial['id'] for row in history))
        dirty_x=self.edit(d,third,3)
        rejected=self.call('switch_data_project_branch',{'dataProjectId':p,'branch':'feature'},error=True)
        self.check('dirty switch is rejected and saved draft remains',bool(rejected) and self.state(d,third)['objects'][0]['bbox']['x']==dirty_x)
        final=self.call('commit_data_project_branch',{'dataProjectId':p,'branch':'main','message':'保存受保护草稿'})
        self.check('final tree is clean',not self.call('get_data_project_working_tree',{'dataProjectId':p})['changes'])
        self.call('create_data_project_branch',{'dataProjectId':p,'name':'delete-check','fromCommitId':final['id']})
        self.call('delete_data_project_branch',{'dataProjectId':p,'branch':'delete-check'})
        self.check('merged disposable branch deletes without changing history',not any(row['name']=='delete-check' for row in self.call('list_data_project_branches',{'dataProjectId':p})))
        protected=self.call('delete_data_project_branch',{'dataProjectId':p,'branch':'main'},error=True)
        self.check('main branch remains protected from deletion',bool(protected) and any(row['name']=='main' and row['headCommitId']==final['id'] for row in self.call('list_data_project_branches',{'dataProjectId':p})))
        self.check('all original imported image bytes are preserved',all(Path(path).is_file() and hashlib.sha256(Path(path).read_bytes()).hexdigest()==digest for path,digest in originals.items()))
        return {'passed':True,'checkedAt':datetime.datetime.now(datetime.timezone.utc).isoformat(),'nativeBackend':self.base,'projectId':p,'datasetIds':[x['id'] for x in datasets],'initialCommitId':initial['id'],'finalCommitId':final['id'],'checks':self.checks,'sourceImageHashes':originals}

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--base-url',required=True);parser.add_argument('--output',required=True);args=parser.parse_args()
    result=Acceptance(args.base_url).run();Path(args.output).write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
if __name__=='__main__':main()
