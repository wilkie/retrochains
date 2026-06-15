struct Item { int code; };
struct Item items[5];
int fetch(int i) {
  return items[i].code;
}
