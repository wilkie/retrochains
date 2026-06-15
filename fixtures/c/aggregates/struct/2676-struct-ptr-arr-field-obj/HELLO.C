struct Bag { int n; int data[4]; };
int get(struct Bag *b) {
  return b->data[2];
}
