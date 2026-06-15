struct Item { int code; int weight; };
struct Item items[3] = {
  { 1, 10 },
  { 2, 20 },
  { 3, 30 }
};
int main(void) {
  return items[1].weight;
}
