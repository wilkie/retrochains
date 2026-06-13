struct V { int data[3]; };
struct V g;
int peek(int i) {
  return g.data[i];
}
