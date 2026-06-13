struct Pair { int a; int b; };
struct Pair make(void);
int main(void) {
  return make().b;
}
