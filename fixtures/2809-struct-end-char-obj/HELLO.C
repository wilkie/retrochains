struct T { int a; char b; };
struct T t = { 100, 'X' };
int main(void) {
  return t.b;
}
