#define FOO 42
int main(void) {
#if defined(FOO) && !defined(BAR)
  return FOO;
#else
  return 0;
#endif
}
