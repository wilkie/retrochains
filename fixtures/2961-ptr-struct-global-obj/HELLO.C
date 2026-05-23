struct P { int x; int y; };
struct P data = { 10, 20 };
struct P *ptr = &data;
int peek(void) {
  return ptr->y;
}
