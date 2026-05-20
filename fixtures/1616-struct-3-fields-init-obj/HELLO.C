struct T { int x; int y; int z; };
int main(void) {
  struct T t = {10, 20, 30};
  return t.x + t.y + t.z;
}
