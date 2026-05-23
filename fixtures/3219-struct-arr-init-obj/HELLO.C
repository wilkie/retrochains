struct M { int tag; int data[3]; };
struct M m = { 5, { 10, 20, 30 } };
int peek(void) {
  return m.data[1];
}
