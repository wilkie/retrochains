struct Box { int tag; int data[3]; };
struct Box b;
int main(void) {
  b.tag = 7;
  b.data[1] = 42;
  return b.data[1];
}
