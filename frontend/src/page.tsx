import { createRoot } from "react-dom/client";
import * as types from "./bindings/index";
import { useCallback, useRef, useState } from "react";

interface PageItemListRowProps {
  pageRoot: string;
  item: types.PageItem;
}
function PageItemListRow({ pageRoot, item }: PageItemListRowProps) {
  let className = item.kind == "Dir" ? "dir" : "file";
  let icon = item.kind == "Dir" ? <>📁</> : <>📃</>;
  if (item.thumbnail_name !== null) {
    icon = <img loading="lazy" src={pageRoot + "/.dop/thumbnail/" + item.thumbnail_name} />;
  }

  let link =
    item.kind == "Dir" ? (
      <a href={item.filename}>{item.basename}</a>
    ) : (
      <a href={item.filename} target="_blank" rel="noopener noreferrer">
        {item.basename}
      </a>
    );

  return (
    <div className={`${className} row`}>
      <div className={`${className} icon`}>{icon}</div>
      <div className={`${className} filename`}>{link}</div>
      <div className={`${className} created`}>{item.created}</div>
      <div className={`${className} modified`}>{item.modified}</div>
      <div className={`${className} accessed`}>{item.accessed}</div>
    </div>
  );
}

type SortCol = "filename" | "created" | "modified" | "accessed";
type SortOrder = "dirsFirst" | "asc" | "desc";

interface PageItemListProps {
  items: types.PageItem[];
  pathSep: string;
  fileDir: string;
  itemsInSubdirs: number;
  pageRoot: string;
}
function PageItemList({ items, pathSep, fileDir, itemsInSubdirs, pageRoot }: PageItemListProps) {
  let searchbox = useRef<HTMLInputElement | null>(null);
  let [searchInput, setSearchInput] = useState("");
  let [caseSensitive, setCaseSensitive] = useState(false);

  // TODO move to new component?
  let [isSearchingEverywhere, setIsSearchingEverywhere] = useState(false);
  let [searchResultsLoading, setSearchResultsLoading] = useState(false);
  let [searchError, setSearchError] = useState<string | undefined>();
  let [searchResults, setSearchResults] = useState<types.SearchResult[]>([]);
  function clearSearchEverywhere() {
    setIsSearchingEverywhere(false);
    setSearchResultsLoading(false);
    setSearchError(undefined);
    setSearchResults([]);
  }

  let doSearchEverywhere = useCallback(() => {
    let path =
      pageRoot +
      "/.dop/search?regex=" +
      encodeURIComponent(searchInput) +
      "&" +
      (caseSensitive ? "case_insensitive=false" : "case_sensitive=true");

    setSearchResults([]);
    setSearchResultsLoading(true);
    setIsSearchingEverywhere(true);
    fetch(path)
      .then((response) => {
        setSearchResultsLoading(false);
        if (response.ok) {
          response
            .json()
            .then((results) => {
              setSearchResults(results);
              setSearchError(undefined);
            })
            .catch((_) => {
              setSearchResults([]);
              setSearchError("invalid JSON from server");
            });
        } else {
          setSearchResults([]);
          response
            .text()
            .then(setSearchError)
            .catch((_) => setSearchError("couldn't read response body"));
        }
      })
      .catch((e) => {
        setSearchResultsLoading(false);
        setSearchResults([]);
        setSearchError("couldn't search: " + (e instanceof Error ? e.message : "unknown error"));
      });
  }, [searchInput, caseSensitive]);

  document.onkeydown = (e) => {
    if (e.key === "s" && document.activeElement?.id !== searchbox.current?.id) {
      e.preventDefault();
      searchbox.current?.focus();
    }
    if (e.key === "Escape") {
      setSearchInput("");
      clearSearchEverywhere();
    }
  };

  let regexWarning = <></>;
  let reg = undefined;
  try {
    let flags = caseSensitive ? "" : "i";
    reg = new RegExp(searchInput, flags);
  } catch (e) {
    regexWarning = <div className="warning">invalid regex: {e instanceof Error ? e.message : "unknown"}</div>;
  }

  let searchWidget = (
    <div id="searchouter">
      <div id="searchboxdiv">
        <button
          id="clearSearch"
          onClick={(_) => {
            setSearchInput("");
            setCaseSensitive(false);
            clearSearchEverywhere();
          }}
        >
          clear search
        </button>

        <input
          id="searchbox"
          ref={searchbox}
          type="text"
          placeholder="🔎 search"
          value={searchInput}
          onChange={(e) => {
            let newSearchInput = e.target.value;
            setSearchInput(newSearchInput);
          }}
          onKeyDown={(e) => {
            if (((!isSearchingEverywhere && e.shiftKey) || isSearchingEverywhere) && e.key === "Enter") {
              doSearchEverywhere();
            }
            if (e.key === "Escape") {
              // if no results, clear?
              e.stopPropagation();
              searchbox.current?.blur();
            }
          }}
        />

        <label id="caseSensitiveLabel" htmlFor="caseSensitive">
          case sensitive?
          <input
            id="caseSensitive"
            type="checkbox"
            checked={caseSensitive}
            onChange={(e) => setCaseSensitive(e.target.checked)}
          />
        </label>

        <button id="searchEverywhere" disabled={searchInput === ""} onClick={(_) => doSearchEverywhere()}>
          {isSearchingEverywhere ? "🛰️ searching everywhere" : "search everywhere"}
        </button>
      </div>

      {regexWarning}
    </div>
  );

  let noResultsText = "no results" + (isSearchingEverywhere ? " anywhere 🛰️" : "");
  if (items.length === 0) {
    noResultsText = "nothing here";
  }
  let noResults = (
    <div id="noresults" className="centerme">
      {noResultsText}
    </div>
  );

  let [sortCol, setSortCol] = useState<SortCol>("filename");
  let [sortOrder, setSortOrder] = useState<SortOrder>("dirsFirst");

  function doSort(oldItems: types.PageItem[]): types.PageItem[] {
    if (sortOrder === "dirsFirst") {
      return items;
    }
    if (sortCol === "filename") {
      if (sortOrder === "asc") {
        return [...oldItems].sort((a, b) => a[sortCol].toUpperCase().localeCompare(b[sortCol].toUpperCase()));
      }
      if (sortOrder === "desc") {
        return [...oldItems].sort(
          (a, b) => -a[sortCol].toUpperCase().localeCompare(b[sortCol].toUpperCase()),
        );
      }
    } else {
      if (sortOrder === "asc") {
        return [...oldItems].sort((a, b) => new Date(a[sortCol]).getTime() - new Date(b[sortCol]).getTime());
      }
      if (sortOrder === "desc") {
        return [...oldItems].sort((a, b) => new Date(b[sortCol]).getTime() - new Date(a[sortCol]).getTime());
      }
    }
    console.error("unexpected value for orderBy", sortOrder);
    return [];
  }

  function nextSortOrder(sortCol: SortCol, sortOrder: SortOrder): SortOrder {
    if (sortCol === "filename") {
      if (sortOrder === "dirsFirst") return "asc";
      if (sortOrder === "asc") return "desc";
      if (sortOrder === "desc") return "dirsFirst";
    }
    if (sortOrder === "dirsFirst") return "desc";
    if (sortOrder === "desc") return "asc";
    if (sortOrder === "asc") return "dirsFirst";
    console.error("unexpected value for orderBy", sortOrder);
    return "dirsFirst";
  }

  let filteredItems = doSort(items);
  if (searchInput !== "" && reg !== undefined) {
    filteredItems = filteredItems.filter((item) => item.filename.search(reg) >= 0);
  }

  let headers: SortCol[] = ["filename", "created", "modified", "accessed"];
  let headerCols = headers.map((col, i) => (
    <div
      key={i}
      className={"header " + col + (col === sortCol ? " " + sortOrder : "")}
      onClick={(_) => {
        if (col === sortCol) {
          setSortOrder(nextSortOrder(col, sortOrder));
        } else {
          setSortCol(col);
          setSortOrder(nextSortOrder(col, "dirsFirst"));
        }
      }}
    >
      {col}
    </div>
  ));

  if (isSearchingEverywhere) {
    let results;
    if (searchResultsLoading) {
      results = (
        <div className="centerme">
          <svg width="24" height="24" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
            <style>
              .spinner_P7sC{"{"}transform-origin:center;animation:spinner_svv2 .75s infinite linear{"}"}
              @keyframes spinner_svv2{"{"}100%{"{"}
              transform:rotate(360deg){"}"}
              {"}"}
            </style>
            <path
              d="M10.14,1.16a11,11,0,0,0-9,8.92A1.59,1.59,0,0,0,2.46,12,1.52,1.52,0,0,0,4.11,10.7a8,8,0,0,1,6.66-6.61A1.42,1.42,0,0,0,12,2.69h0A1.57,1.57,0,0,0,10.14,1.16Z"
              className="spinner_P7sC"
            />
          </svg>
        </div>
      );
    } else if (searchError !== undefined) {
      results = <div className="warning">{searchError}</div>;
    } else if (searchResults.length === 0) {
      results = noResults;
    } else {
      results = (
        <>
          {searchResults.map((result, i) => {
            let parts = result.path.split(pathSep);

            let href = pageRoot;
            let as = [<a href={href}>{fileDir}</a>];
            parts.forEach((part, i) => {
              href += "/" + part;

              let target = undefined;
              if (i + 1 === parts.length && result.kind === "File") {
                target = "_blank"
              }

              as.push(
                <a href={href} target={target}>
                  {part}
                </a>,
              );
            });

            let combined = as.reduce((resultLinks, a) => (
              <>
                {resultLinks} {pathSep} {a}
              </>
            ));

            let icon = result.kind === "Dir" ? <>📁</> : <>📃</>;
            return (
              <div key={i} className="everywheresearch">
                {icon} {combined}
              </div>
            );
          })}
        </>
      );
    }

    return (
      <>
        {searchWidget}
        {results}
      </>
    );
  }

  let numDirs = items.filter((item) => item.kind === "Dir").length;

  return (
    <>
      {searchWidget}

      <div className="filetable">
        <div className="header row">
          <div className="header" id="topleft"></div>
          {headerCols}
        </div>
        {filteredItems.map((item) => (
          <PageItemListRow key={item.basename} item={item} pageRoot={pageRoot} />
        ))}
      </div>

      <div id="numfiles">
        {searchInput !== "" ? <>{filteredItems.length} of</> : ""}
        {items.length}
        {numDirs !== 0 ? <>/ {itemsInSubdirs} </> : ""}
      </div>

      {filteredItems.length === 0 ? noResults : ""}
    </>
  );
}

let items: types.PageItem[] = JSON.parse(document.getElementById("items")!.innerText);
let pathSep = document.getElementById("pathSep")!.innerText;
let fileDir = document.getElementById("fileDir")!.innerText;
let itemsInSubdirs = parseInt(document.getElementById("itemsInSubdirs")!.innerText);
let pageRoot = document.getElementById("pageRoot")!.innerText;

let root = document.getElementById("reactRoot")!;
createRoot(root).render(
  <PageItemList
    items={items}
    pathSep={pathSep}
    fileDir={fileDir}
    itemsInSubdirs={itemsInSubdirs}
    pageRoot={pageRoot}
  />,
);
