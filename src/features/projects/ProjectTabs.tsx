import { projectTabIcons, type ProjectTab } from "../../shared/routes";

export function ProjectTabs({
  tabs,
  activeTab,
  onChange,
}: {
  tabs: ProjectTab[];
  activeTab: ProjectTab;
  onChange: (tab: ProjectTab) => void;
}) {
  return (
    <nav className="project-tabs" aria-label="项目页面">
      {tabs.map((item) => {
        const TabIcon = projectTabIcons[item];
        return <button aria-pressed={item === activeTab} className={item === activeTab ? "active" : ""} key={item} onClick={() => onChange(item)} type="button"><TabIcon aria-hidden="true" size={15} />{item}</button>;
      })}
    </nav>
  );
}
